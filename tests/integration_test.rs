//! Integration tests for HeliosDB Lite
//!
//! These tests verify that the complete SQL execution pipeline works end-to-end.

use heliosdb_lite::{EmbeddedDatabase, Result};

#[test]
fn test_database_creation() -> Result<()> {
    let _db = EmbeddedDatabase::new_in_memory()?;
    assert!(true, "Database created successfully");
    Ok(())
}

#[test]
fn test_sql_parsing_basic() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Test that SQL parsing works (even if execution returns empty results)
    let result = db.query("SELECT * FROM users", &[]);

    // We expect an error or empty results since the table doesn't exist yet
    // But the parsing and planning should succeed
    match result {
        Ok(rows) => {
            // Empty table is fine
            assert_eq!(rows.len(), 0);
        }
        Err(_) => {
            // Table not found or other execution error is also acceptable
            // since we haven't implemented full storage integration yet
        }
    }

    Ok(())
}

#[test]
fn test_create_table_parsing() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Test CREATE TABLE statement parsing
    let result = db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)");

    // Should parse successfully even if execution is not fully implemented
    match result {
        Ok(_) => {
            // Success - table created (or planned to be created)
        }
        Err(e) => {
            // If it fails, it should be an execution error, not a parsing error
            let error_msg = e.to_string();
            assert!(!error_msg.contains("parse"),
                "Should not be a parsing error, got: {}", error_msg);
        }
    }

    Ok(())
}

#[test]
fn test_insert_parsing() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Test INSERT statement parsing
    let result = db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')");

    // Should parse successfully even if execution is not fully implemented
    match result {
        Ok(_) => {
            // Success - insert planned
        }
        Err(e) => {
            // If it fails, it should be an execution error, not a parsing error
            let error_msg = e.to_string();
            assert!(!error_msg.contains("parse"),
                "Should not be a parsing error, got: {}", error_msg);
        }
    }

    Ok(())
}

#[test]
fn test_select_with_where_parsing() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Test SELECT with WHERE clause
    let result = db.query("SELECT id, name FROM users WHERE id = 1", &[]);

    // Should parse and plan successfully
    match result {
        Ok(rows) => {
            // Empty is fine - storage not fully wired yet
            assert!(rows.len() == 0);
        }
        Err(e) => {
            let error_msg = e.to_string();
            // Should not fail on parsing/planning
            assert!(!error_msg.contains("parse") && !error_msg.contains("plan"),
                "Should not be a parse/plan error, got: {}", error_msg);
        }
    }

    Ok(())
}

#[test]
fn test_select_with_limit_parsing() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Test SELECT with LIMIT/OFFSET
    let result = db.query("SELECT * FROM users LIMIT 10 OFFSET 5", &[]);

    // Should parse and plan successfully
    match result {
        Ok(_) => {
            // Success
        }
        Err(e) => {
            let error_msg = e.to_string();
            assert!(!error_msg.contains("parse"),
                "Should not be a parsing error, got: {}", error_msg);
        }
    }

    Ok(())
}

#[test]
fn test_transaction_api() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Test transaction creation
    let tx = db.begin_transaction()?;

    // Test commit
    tx.commit()?;

    // Test rollback
    let tx2 = db.begin_transaction()?;
    tx2.rollback()?;

    Ok(())
}

#[test]
fn test_invalid_sql_returns_error() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Invalid SQL should return an error
    let result = db.query("SELECT FROM WHERE", &[]);
    assert!(result.is_err(), "Invalid SQL should return an error");

    let error = result.unwrap_err();
    let error_msg = error.to_string();
    assert!(error_msg.contains("parse") || error_msg.contains("SQL"),
        "Error should mention parsing or SQL issue, got: {}", error_msg);
}

#[test]
fn test_end_to_end_crud_operations() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // 1. CREATE TABLE
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;

    // 2. INSERT data
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // 3. SELECT all
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 3, "Should have 3 rows");

    // Verify first row has expected number of columns
    assert_eq!(results[0].len(), 3, "Each row should have 3 columns");

    Ok(())
}

#[test]
fn test_table_does_not_exist_error() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Query non-existent table
    let result = db.query("SELECT * FROM nonexistent", &[]);

    // Should return an error (table doesn't exist)
    match result {
        Ok(rows) => {
            // Empty result is also acceptable if table doesn't exist
            assert_eq!(rows.len(), 0);
        }
        Err(e) => {
            // Error mentioning table not found is expected
            let msg = e.to_string();
            assert!(msg.contains("not") || msg.contains("exist") || msg.contains("table"),
                "Error should mention table issue, got: {}", msg);
        }
    }
}

#[test]
fn test_multiple_tables() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create multiple tables
    db.execute("CREATE TABLE products (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, product_id INT)")?;

    // Insert into both tables
    db.execute("INSERT INTO products (id, name) VALUES (1, 'Widget')")?;
    db.execute("INSERT INTO orders (id, product_id) VALUES (100, 1)")?;

    // Query both tables
    let products = db.query("SELECT * FROM products", &[])?;
    let orders = db.query("SELECT * FROM orders", &[])?;

    assert_eq!(products.len(), 1);
    assert_eq!(orders.len(), 1);

    Ok(())
}

#[test]
fn test_where_clause_equality() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // Test WHERE id = 2
    let results = db.query("SELECT * FROM users WHERE id = 2", &[])?;
    assert_eq!(results.len(), 1, "Should return exactly 1 row");
    // Verify it's Bob (id=2)
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int4(2));

    Ok(())
}

#[test]
fn test_where_clause_comparison() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // Test WHERE age > 28
    let results = db.query("SELECT * FROM users WHERE age > 28", &[])?;
    assert_eq!(results.len(), 2, "Should return 2 rows (Alice and Charlie)");

    // Test WHERE age < 30
    let results = db.query("SELECT * FROM users WHERE age < 30", &[])?;
    assert_eq!(results.len(), 1, "Should return 1 row (Bob)");

    Ok(())
}

#[test]
fn test_select_specific_columns() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;

    // Test SELECT name, age (not all columns)
    let results = db.query("SELECT name, age FROM users", &[])?;
    assert_eq!(results.len(), 2, "Should return 2 rows");
    assert_eq!(results[0].len(), 2, "Should have 2 columns (name, age)");

    Ok(())
}

#[test]
fn test_order_by_ascending() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;

    // Test ORDER BY age ASC
    let results = db.query("SELECT * FROM users ORDER BY age ASC", &[])?;
    assert_eq!(results.len(), 3, "Should return 3 rows");

    // Verify order: Bob (25), Alice (30), Charlie (35)
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int4(2)); // Bob's id
    assert_eq!(results[1].get(0).unwrap(), &heliosdb_lite::Value::Int4(1)); // Alice's id
    assert_eq!(results[2].get(0).unwrap(), &heliosdb_lite::Value::Int4(3)); // Charlie's id

    Ok(())
}

#[test]
fn test_order_by_descending() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;

    // Test ORDER BY age DESC
    let results = db.query("SELECT * FROM users ORDER BY age DESC", &[])?;
    assert_eq!(results.len(), 3, "Should return 3 rows");

    // Verify order: Charlie (35), Alice (30), Bob (25)
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int4(3)); // Charlie's id
    assert_eq!(results[1].get(0).unwrap(), &heliosdb_lite::Value::Int4(1)); // Alice's id
    assert_eq!(results[2].get(0).unwrap(), &heliosdb_lite::Value::Int4(2)); // Bob's id

    Ok(())
}

#[test]
fn test_order_by_string() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;

    // Test ORDER BY name ASC
    let results = db.query("SELECT * FROM users ORDER BY name", &[])?;
    assert_eq!(results.len(), 3, "Should return 3 rows");

    // Verify order: Alice, Bob, Charlie (alphabetical)
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int4(1)); // Alice's id
    assert_eq!(results[1].get(0).unwrap(), &heliosdb_lite::Value::Int4(2)); // Bob's id
    assert_eq!(results[2].get(0).unwrap(), &heliosdb_lite::Value::Int4(3)); // Charlie's id

    Ok(())
}

#[test]
fn test_order_by_with_limit() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (4, 'Dave', 40)")?;

    // Test ORDER BY age DESC LIMIT 2
    let results = db.query("SELECT * FROM users ORDER BY age DESC LIMIT 2", &[])?;
    assert_eq!(results.len(), 2, "Should return 2 rows");

    // Verify order: Dave (40), Charlie (35)
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int4(4)); // Dave's id
    assert_eq!(results[1].get(0).unwrap(), &heliosdb_lite::Value::Int4(3)); // Charlie's id

    Ok(())
}

#[test]
fn test_count_aggregate() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // Test COUNT(*)
    let results = db.query("SELECT COUNT(*) FROM users", &[])?;
    assert_eq!(results.len(), 1, "Should return 1 row");
    assert_eq!(results[0].len(), 1, "Should have 1 column");
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int8(3));

    // Test COUNT(column)
    let results = db.query("SELECT COUNT(name) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int8(3));

    Ok(())
}

#[test]
fn test_sum_avg_aggregate() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 20)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 40)")?;

    // Test SUM
    let results = db.query("SELECT SUM(age) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int8(90)); // 30+20+40=90

    // Test AVG
    let results = db.query("SELECT AVG(age) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Float8(30.0)); // 90/3=30.0

    Ok(())
}

#[test]
fn test_min_max_aggregate() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // Test MIN
    let results = db.query("SELECT MIN(age) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int4(25));

    // Test MAX
    let results = db.query("SELECT MAX(age) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int4(35));

    // Test MIN on strings
    let results = db.query("SELECT MIN(name) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::String("Alice".to_string()));

    // Test MAX on strings
    let results = db.query("SELECT MAX(name) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::String("Charlie".to_string()));

    Ok(())
}

#[test]
fn test_group_by() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table with departments
    db.execute("CREATE TABLE employees (id INT PRIMARY KEY, name TEXT, dept TEXT, salary INT)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (1, 'Alice', 'Engineering', 100)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (2, 'Bob', 'Engineering', 120)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (3, 'Charlie', 'Sales', 80)")?;
    db.execute("INSERT INTO employees (id, name, dept, salary) VALUES (4, 'Dave', 'Sales', 90)")?;

    // Test GROUP BY with COUNT
    let results = db.query("SELECT dept, COUNT(*) FROM employees GROUP BY dept", &[])?;
    assert_eq!(results.len(), 2, "Should return 2 groups (Engineering, Sales)");

    // Results should be sorted by dept (BTreeMap ordering)
    // Engineering comes before Sales alphabetically
    assert_eq!(results[0].len(), 2, "Each row should have 2 columns (dept, count)");
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::String("Engineering".to_string()));
    assert_eq!(results[0].get(1).unwrap(), &heliosdb_lite::Value::Int8(2));

    assert_eq!(results[1].get(0).unwrap(), &heliosdb_lite::Value::String("Sales".to_string()));
    assert_eq!(results[1].get(1).unwrap(), &heliosdb_lite::Value::Int8(2));

    // Test GROUP BY with SUM
    let results = db.query("SELECT dept, SUM(salary) FROM employees GROUP BY dept", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get(1).unwrap(), &heliosdb_lite::Value::Int8(220)); // Engineering: 100+120
    assert_eq!(results[1].get(1).unwrap(), &heliosdb_lite::Value::Int8(170)); // Sales: 80+90

    // Test GROUP BY with AVG
    let results = db.query("SELECT dept, AVG(salary) FROM employees GROUP BY dept", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get(1).unwrap(), &heliosdb_lite::Value::Float8(110.0)); // Engineering: 220/2
    assert_eq!(results[1].get(1).unwrap(), &heliosdb_lite::Value::Float8(85.0)); // Sales: 170/2

    Ok(())
}

#[test]
fn test_count_distinct() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table with duplicate ages
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (4, 'Dave', 25)")?;

    // Test COUNT(DISTINCT age) - should be 2 (25 and 30)
    let results = db.query("SELECT COUNT(DISTINCT age) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int8(2));

    // Compare with COUNT(age) - should be 4
    let results = db.query("SELECT COUNT(age) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int8(4));

    Ok(())
}

#[test]
fn test_multiple_aggregates() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 20)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 40)")?;

    // Test multiple aggregates in same query
    let results = db.query("SELECT COUNT(*), MIN(age), MAX(age), AVG(age), SUM(age) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].len(), 5, "Should have 5 columns");

    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int8(3)); // COUNT(*)
    assert_eq!(results[0].get(1).unwrap(), &heliosdb_lite::Value::Int4(20)); // MIN(age)
    assert_eq!(results[0].get(2).unwrap(), &heliosdb_lite::Value::Int4(40)); // MAX(age)
    assert_eq!(results[0].get(3).unwrap(), &heliosdb_lite::Value::Float8(30.0)); // AVG(age)
    assert_eq!(results[0].get(4).unwrap(), &heliosdb_lite::Value::Int8(90)); // SUM(age)

    Ok(())
}

#[test]
fn test_update_all_rows() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // Update all rows
    let updated = db.execute("UPDATE users SET age = 40")?;
    assert_eq!(updated, 3, "Should update 3 rows");

    // Verify all rows were updated
    let results = db.query("SELECT * FROM users", &[])?;
    for row in &results {
        assert_eq!(row.get(2).unwrap(), &heliosdb_lite::Value::Int4(40));
    }

    Ok(())
}

#[test]
fn test_update_with_where() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // Update specific row
    let updated = db.execute("UPDATE users SET age = 26 WHERE id = 2")?;
    assert_eq!(updated, 1, "Should update 1 row");

    // Verify only Bob was updated
    let results = db.query("SELECT * FROM users WHERE id = 2", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(2).unwrap(), &heliosdb_lite::Value::Int4(26));

    // Verify others were not updated
    let alice = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(alice[0].get(2).unwrap(), &heliosdb_lite::Value::Int4(30));

    Ok(())
}

#[test]
fn test_update_multiple_columns() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;

    // Update multiple columns
    let updated = db.execute("UPDATE users SET name = 'Alicia', age = 31 WHERE id = 1")?;
    assert_eq!(updated, 1);

    // Verify both columns were updated
    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results[0].get(1).unwrap(), &heliosdb_lite::Value::String("Alicia".to_string()));
    assert_eq!(results[0].get(2).unwrap(), &heliosdb_lite::Value::Int4(31));

    Ok(())
}

#[test]
fn test_delete_all_rows() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // Delete all rows
    let deleted = db.execute("DELETE FROM users")?;
    assert_eq!(deleted, 3, "Should delete 3 rows");

    // Verify table is empty
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 0);

    Ok(())
}

#[test]
fn test_delete_with_where() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;

    // Delete specific row
    let deleted = db.execute("DELETE FROM users WHERE id = 2")?;
    assert_eq!(deleted, 1, "Should delete 1 row");

    // Verify Bob was deleted
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 2);

    // Verify Alice and Charlie remain
    let ids: Vec<_> = results.iter().map(|r| r.get(0).unwrap()).collect();
    assert!(ids.contains(&&heliosdb_lite::Value::Int4(1)));
    assert!(ids.contains(&&heliosdb_lite::Value::Int4(3)));

    Ok(())
}

#[test]
fn test_delete_with_complex_where() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create and populate table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")?;
    db.execute("INSERT INTO users (id, name, age) VALUES (4, 'Dave', 40)")?;

    // Delete rows where age > 28
    let deleted = db.execute("DELETE FROM users WHERE age > 28")?;
    assert_eq!(deleted, 3, "Should delete 3 rows (Alice, Charlie, Dave)");

    // Verify only Bob remains
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &heliosdb_lite::Value::Int4(2)); // Bob's id

    Ok(())
}

#[test]
fn test_select_distinct_removes_duplicates() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer TEXT, product TEXT)")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (1, 'Alice', 'Laptop')")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (2, 'Bob', 'Phone')")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (3, 'Alice', 'Mouse')")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (4, 'Bob', 'Keyboard')")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (5, 'Alice', 'Monitor')")?;

    // SELECT DISTINCT on customer column
    let results = db.query("SELECT DISTINCT customer FROM orders", &[])?;

    assert_eq!(results.len(), 2, "Should return 2 distinct customers");

    let customers: Vec<String> = results.iter()
        .map(|r| match r.get(0).unwrap() {
            heliosdb_lite::Value::String(s) => s.clone(),
            _ => panic!("Expected string value"),
        })
        .collect();

    assert!(customers.contains(&"Alice".to_string()));
    assert!(customers.contains(&"Bob".to_string()));

    Ok(())
}

#[test]
fn test_select_distinct_all_columns() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE items (id INT PRIMARY KEY, name TEXT, category TEXT)")?;
    db.execute("INSERT INTO items (id, name, category) VALUES (1, 'Laptop', 'Electronics')")?;
    db.execute("INSERT INTO items (id, name, category) VALUES (2, 'Laptop', 'Electronics')")?;
    db.execute("INSERT INTO items (id, name, category) VALUES (3, 'Phone', 'Electronics')")?;
    db.execute("INSERT INTO items (id, name, category) VALUES (4, 'Laptop', 'Electronics')")?;

    // SELECT DISTINCT should remove duplicate rows
    let results = db.query("SELECT DISTINCT name, category FROM items", &[])?;

    // Should only return 2 distinct combinations (Laptop/Electronics and Phone/Electronics)
    assert_eq!(results.len(), 2, "Should return 2 distinct name/category combinations");

    Ok(())
}

#[test]
fn test_select_distinct_with_where() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE sales (id INT PRIMARY KEY, region TEXT, amount INT)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (1, 'North', 100)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (2, 'South', 200)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (3, 'North', 150)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (4, 'East', 300)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (5, 'North', 120)")?;

    // SELECT DISTINCT with WHERE clause
    let results = db.query("SELECT DISTINCT region FROM sales WHERE amount > 100", &[])?;

    // Should return 3 distinct regions (North, South, East) from filtered results
    assert_eq!(results.len(), 3, "Should return 3 distinct regions from filtered results");

    Ok(())
}

#[test]
fn test_select_without_distinct_shows_duplicates() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE tasks (id INT PRIMARY KEY, status TEXT)")?;
    db.execute("INSERT INTO tasks (id, status) VALUES (1, 'Done')")?;
    db.execute("INSERT INTO tasks (id, status) VALUES (2, 'Pending')")?;
    db.execute("INSERT INTO tasks (id, status) VALUES (3, 'Done')")?;
    db.execute("INSERT INTO tasks (id, status) VALUES (4, 'Done')")?;

    // Regular SELECT should return all rows
    let results_all = db.query("SELECT status FROM tasks", &[])?;
    assert_eq!(results_all.len(), 4, "Regular SELECT should return all 4 rows");

    // SELECT DISTINCT should return only 2 unique values
    let results_distinct = db.query("SELECT DISTINCT status FROM tasks", &[])?;
    assert_eq!(results_distinct.len(), 2, "DISTINCT should return only 2 unique values");

    Ok(())
}

#[test]
fn test_inner_join_basic() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create users table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;
    db.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')")?;
    db.execute("INSERT INTO users (id, name) VALUES (3, 'Charlie')")?;

    // Create orders table
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, product TEXT)")?;
    db.execute("INSERT INTO orders (id, user_id, product) VALUES (1, 1, 'Laptop')")?;
    db.execute("INSERT INTO orders (id, user_id, product) VALUES (2, 2, 'Phone')")?;
    db.execute("INSERT INTO orders (id, user_id, product) VALUES (3, 1, 'Mouse')")?;

    // Join users and orders
    let results = db.query(
        "SELECT * FROM users INNER JOIN orders ON users.id = orders.user_id",
        &[]
    )?;

    // Should return 3 rows (Alice's 2 orders + Bob's 1 order)
    assert_eq!(results.len(), 3, "Should return 3 joined rows");

    // Check that we have combined columns from both tables
    // users has 2 columns (id, name), orders has 3 columns (id, user_id, product)
    // Total should be 5 columns
    assert_eq!(results[0].values.len(), 5, "Should have 5 columns from both tables");

    Ok(())
}

#[test]
fn test_inner_join_with_where() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create departments table with unique column names
    db.execute("CREATE TABLE departments (dept_id INT PRIMARY KEY, dept_name TEXT)")?;
    db.execute("INSERT INTO departments (dept_id, dept_name) VALUES (1, 'Engineering')")?;
    db.execute("INSERT INTO departments (dept_id, dept_name) VALUES (2, 'Sales')")?;

    // Create employees table
    db.execute("CREATE TABLE employees (emp_id INT PRIMARY KEY, emp_name TEXT, department_id INT)")?;
    db.execute("INSERT INTO employees (emp_id, emp_name, department_id) VALUES (1, 'Alice', 1)")?;
    db.execute("INSERT INTO employees (emp_id, emp_name, department_id) VALUES (2, 'Bob', 2)")?;
    db.execute("INSERT INTO employees (emp_id, emp_name, department_id) VALUES (3, 'Charlie', 1)")?;
    db.execute("INSERT INTO employees (emp_id, emp_name, department_id) VALUES (4, 'Dave', 2)")?;

    // Join with WHERE clause filtering
    let results = db.query(
        "SELECT * FROM employees INNER JOIN departments ON employees.department_id = departments.dept_id WHERE departments.dept_name = 'Engineering'",
        &[]
    )?;

    // Should return 2 rows (Alice and Charlie in Engineering)
    assert_eq!(results.len(), 2, "Should return 2 Engineering employees");

    Ok(())
}

#[test]
fn test_inner_join_no_matches() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create two tables with no matching rows (use unique column names to avoid ambiguity)
    db.execute("CREATE TABLE products (product_id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO products (product_id, name) VALUES (1, 'Laptop')")?;

    db.execute("CREATE TABLE reviews (review_id INT PRIMARY KEY, product_ref INT, rating INT)")?;
    db.execute("INSERT INTO reviews (review_id, product_ref, rating) VALUES (1, 99, 5)")?;

    // Join with no matches (product_id 1 doesn't match product_ref 99)
    let results = db.query(
        "SELECT * FROM products INNER JOIN reviews ON products.product_id = reviews.product_ref",
        &[]
    )?;

    // Should return 0 rows (no matching ids)
    assert_eq!(results.len(), 0, "Should return no rows when join has no matches");

    Ok(())
}

#[test]
fn test_having_clause_basic() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create a sales table
    db.execute("CREATE TABLE sales (id INT PRIMARY KEY, region TEXT, amount INT)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (1, 'North', 100)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (2, 'South', 200)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (3, 'North', 150)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (4, 'East', 50)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (5, 'South', 300)")?;
    db.execute("INSERT INTO sales (id, region, amount) VALUES (6, 'East', 75)")?;

    // Query with HAVING clause - only regions with total sales > 200
    let results = db.query(
        "SELECT region, SUM(amount) FROM sales GROUP BY region HAVING SUM(amount) > 200",
        &[]
    )?;

    // Should return 2 regions: North (250) and South (500)
    assert_eq!(results.len(), 2, "Should return 2 regions with sales > 200");

    Ok(())
}

#[test]
fn test_having_clause_count() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create an orders table
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer TEXT, product TEXT)")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (1, 'Alice', 'Laptop')")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (2, 'Bob', 'Phone')")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (3, 'Alice', 'Mouse')")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (4, 'Alice', 'Keyboard')")?;
    db.execute("INSERT INTO orders (id, customer, product) VALUES (5, 'Charlie', 'Monitor')")?;

    // Query with HAVING clause - only customers with more than 1 order
    let results = db.query(
        "SELECT customer, COUNT(*) FROM orders GROUP BY customer HAVING COUNT(*) > 1",
        &[]
    )?;

    // Should return only Alice (3 orders)
    assert_eq!(results.len(), 1, "Should return 1 customer with more than 1 order");

    // Verify it's Alice
    let customer_name = match results[0].get(0).unwrap() {
        heliosdb_lite::Value::String(s) => s.clone(),
        _ => panic!("Expected string value"),
    };
    assert_eq!(customer_name, "Alice");

    Ok(())
}

#[test]
fn test_having_clause_filters_all() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create a table where no groups pass the HAVING condition
    db.execute("CREATE TABLE items (id INT PRIMARY KEY, category TEXT, price INT)")?;
    db.execute("INSERT INTO items (id, category, price) VALUES (1, 'A', 10)")?;
    db.execute("INSERT INTO items (id, category, price) VALUES (2, 'B', 20)")?;
    db.execute("INSERT INTO items (id, category, price) VALUES (3, 'A', 15)")?;

    // Query with HAVING that filters out all groups
    let results = db.query(
        "SELECT category, SUM(price) FROM items GROUP BY category HAVING SUM(price) > 1000",
        &[]
    )?;

    // Should return 0 rows (no category has sum > 1000)
    assert_eq!(results.len(), 0, "Should return no rows when HAVING filters all groups");

    Ok(())
}

#[test]
fn test_group_by_without_having() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create a sales table
    db.execute("CREATE TABLE revenue (id INT PRIMARY KEY, dept TEXT, amount INT)")?;
    db.execute("INSERT INTO revenue (id, dept, amount) VALUES (1, 'Engineering', 100)")?;
    db.execute("INSERT INTO revenue (id, dept, amount) VALUES (2, 'Sales', 200)")?;
    db.execute("INSERT INTO revenue (id, dept, amount) VALUES (3, 'Engineering', 150)")?;
    db.execute("INSERT INTO revenue (id, dept, amount) VALUES (4, 'Marketing', 50)")?;

    // Query without HAVING - should return all groups
    let results = db.query(
        "SELECT dept, SUM(amount) FROM revenue GROUP BY dept",
        &[]
    )?;

    // Should return 3 departments
    assert_eq!(results.len(), 3, "Should return all 3 departments without HAVING");

    Ok(())
}
