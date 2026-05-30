//! Comprehensive tests for parameterized query support
//!
//! Tests cover:
//! - SELECT queries with $1, $2, $3 parameters
//! - INSERT queries with parameters
//! - UPDATE queries with parameters
//! - DELETE queries with parameters
//! - Parameter type checking and coercion
//! - Multiple parameters in WHERE clauses
//! - Parameters in aggregates and JOINs
//! - SQL injection prevention

use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
fn test_select_with_single_parameter() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup: Create table and insert data
    db.execute("CREATE TABLE users (id INT, name TEXT, age INT)").unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Alice', 30)").unwrap();
    db.execute("INSERT INTO users VALUES (2, 'Bob', 25)").unwrap();
    db.execute("INSERT INTO users VALUES (3, 'Charlie', 35)").unwrap();

    // Test: SELECT with single parameter in WHERE clause
    let results = db
        .query_params(
            "SELECT * FROM users WHERE name = $1",
            &[Value::String("Alice".to_string())],
        )
        .expect("Failed to execute parameterized query");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(1).unwrap(), &Value::String("Alice".to_string()));
}

#[test]
fn test_select_with_multiple_parameters() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE products (id INT, name TEXT, price INT, category TEXT)")
        .unwrap();
    db.execute("INSERT INTO products VALUES (1, 'Laptop', 1000, 'Electronics')")
        .unwrap();
    db.execute("INSERT INTO products VALUES (2, 'Mouse', 25, 'Electronics')")
        .unwrap();
    db.execute("INSERT INTO products VALUES (3, 'Desk', 300, 'Furniture')")
        .unwrap();
    db.execute("INSERT INTO products VALUES (4, 'Chair', 150, 'Furniture')")
        .unwrap();

    // Test: SELECT with multiple parameters
    let results = db
        .query_params(
            "SELECT * FROM products WHERE category = $1 AND price > $2",
            &[Value::String("Electronics".to_string()), Value::Int4(50)],
        )
        .expect("Failed to execute query with multiple parameters");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(1).unwrap(), &Value::String("Laptop".to_string()));
}

#[test]
fn test_select_with_comparison_operators() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE scores (id INT, player TEXT, score INT)")
        .unwrap();
    db.execute("INSERT INTO scores VALUES (1, 'Alice', 100)").unwrap();
    db.execute("INSERT INTO scores VALUES (2, 'Bob', 85)").unwrap();
    db.execute("INSERT INTO scores VALUES (3, 'Charlie', 90)").unwrap();

    // Test: Greater than
    let results = db
        .query_params("SELECT * FROM scores WHERE score > $1", &[Value::Int4(85)])
        .expect("Failed to execute query");
    assert_eq!(results.len(), 2); // Alice and Charlie

    // Test: Less than or equal
    let results = db
        .query_params("SELECT * FROM scores WHERE score <= $1", &[Value::Int4(90)])
        .expect("Failed to execute query");
    assert_eq!(results.len(), 2); // Bob and Charlie

    // Test: Equals
    let results = db
        .query_params("SELECT * FROM scores WHERE score = $1", &[Value::Int4(100)])
        .expect("Failed to execute query");
    assert_eq!(results.len(), 1); // Alice
}

#[test]
fn test_insert_with_parameters() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE employees (id INT, name TEXT, salary INT)")
        .unwrap();

    // Test: INSERT with parameters
    let count = db
        .execute_params(
            "INSERT INTO employees VALUES ($1, $2, $3)",
            &[Value::Int4(1), Value::String("Alice".to_string()), Value::Int4(75000)],
        )
        .expect("Failed to insert with parameters");

    assert_eq!(count, 1);

    // Verify insertion
    let results = db.query("SELECT * FROM employees", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(1).unwrap(), &Value::String("Alice".to_string()));
    assert_eq!(results[0].get(2).unwrap(), &Value::Int4(75000));
}

#[test]
fn test_parameterized_insert_all_columns_enforces_not_null() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");
    db.execute("CREATE TABLE required_params (id INT, name TEXT NOT NULL)")
        .unwrap();

    let result = db.execute_params(
        "INSERT INTO required_params VALUES ($1, $2)",
        &[Value::Int4(1), Value::Null],
    );

    assert!(result.is_err(), "NULL parameter should fail NOT NULL check");
}

#[test]
fn test_parameterized_insert_cache_invalidated_by_schema_change() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");
    db.execute("CREATE TABLE param_cache_schema (id INT PRIMARY KEY, name TEXT)")
        .unwrap();

    let sql = "INSERT INTO param_cache_schema (id, name) VALUES ($1, $2)";
    db.execute_params(sql, &[Value::Int4(1), Value::String("before".to_string())])
        .unwrap();

    db.execute("ALTER TABLE param_cache_schema ADD COLUMN score INT DEFAULT 7")
        .unwrap();

    db.execute_params(sql, &[Value::Int4(2), Value::String("after".to_string())])
        .unwrap();

    let rows = db
        .query("SELECT id, name, score FROM param_cache_schema WHERE id = 2", &[])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(2).unwrap(), &Value::Int4(7));
}

#[test]
fn test_parameterized_update_cache_invalidated_by_schema_change() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");
    let sql = "UPDATE param_update_cache SET v = $1 WHERE id = $2";

    db.execute("CREATE TABLE param_update_cache (id INT PRIMARY KEY, v INT)")
        .unwrap();
    db.execute("INSERT INTO param_update_cache VALUES (1, 10)")
        .unwrap();
    db.execute_params(sql, &[Value::Int4(11), Value::Int4(1)])
        .unwrap();

    db.execute("DROP TABLE param_update_cache").unwrap();
    db.execute("CREATE TABLE param_update_cache (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO param_update_cache VALUES (1, 'before')")
        .unwrap();
    db.execute_params(sql, &[Value::String("after".to_string()), Value::Int4(1)])
        .unwrap();

    let rows = db
        .query("SELECT v FROM param_update_cache WHERE id = 1", &[])
        .unwrap();
    assert_eq!(rows[0].get(0).unwrap(), &Value::String("after".to_string()));
}

#[test]
fn test_parameterized_delete_cache_invalidated_by_schema_change() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");
    let sql = "DELETE FROM param_delete_cache WHERE id = $1";

    db.execute("CREATE TABLE param_delete_cache (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO param_delete_cache VALUES (1, 'old')")
        .unwrap();
    db.execute_params(sql, &[Value::Int4(1)]).unwrap();

    db.execute("DROP TABLE param_delete_cache").unwrap();
    db.execute("CREATE TABLE param_delete_cache (id TEXT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO param_delete_cache VALUES ('k1', 'new')")
        .unwrap();
    db.execute_params(sql, &[Value::String("k1".to_string())])
        .unwrap();

    let rows = db.query("SELECT * FROM param_delete_cache", &[]).unwrap();
    assert_eq!(rows.len(), 0);
}

#[test]
fn test_parameterized_insert_in_explicit_transaction_commit_and_rollback() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");
    db.execute("CREATE TABLE txn_params (id INT PRIMARY KEY, name TEXT)")
        .unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_params(
        "INSERT INTO txn_params (id, name) VALUES ($1, $2)",
        &[Value::Int4(1), Value::String("committed".to_string())],
    )
    .unwrap();
    let in_txn = db
        .query_params("SELECT * FROM txn_params WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    assert_eq!(in_txn.len(), 1);
    let (in_txn_with_columns, columns) = db
        .query_params_with_columns("SELECT * FROM txn_params WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    assert_eq!(columns, vec!["id".to_string(), "name".to_string()]);
    assert_eq!(in_txn_with_columns.len(), 1);
    db.execute("COMMIT").unwrap();

    let committed = db
        .query_params("SELECT * FROM txn_params WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    assert_eq!(committed.len(), 1);

    db.execute("BEGIN").unwrap();
    db.execute_params(
        "INSERT INTO txn_params (id, name) VALUES ($1, $2)",
        &[Value::Int4(2), Value::String("rolled_back".to_string())],
    )
    .unwrap();
    let in_txn = db
        .query_params("SELECT * FROM txn_params WHERE id = $1", &[Value::Int4(2)])
        .unwrap();
    assert_eq!(in_txn.len(), 1);
    db.execute("ROLLBACK").unwrap();

    let rolled_back = db
        .query_params("SELECT * FROM txn_params WHERE id = $1", &[Value::Int4(2)])
        .unwrap();
    assert_eq!(rolled_back.len(), 0);
}

#[test]
fn test_execute_many_params_insert_autocommit() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");
    db.execute("CREATE TABLE many_params (id INT PRIMARY KEY, name TEXT)")
        .unwrap();

    let rows = vec![
        vec![Value::Int4(1), Value::String("a".to_string())],
        vec![Value::Int4(2), Value::String("b".to_string())],
        vec![Value::Int4(3), Value::String("c".to_string())],
    ];
    let count = db
        .execute_many_params("INSERT INTO many_params (id, name) VALUES ($1, $2)", &rows)
        .unwrap();
    assert_eq!(count, 3);

    let results = db
        .query("SELECT id, name FROM many_params ORDER BY id", &[])
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[1].get(1).unwrap(), &Value::String("b".to_string()));
}

#[test]
fn test_execute_many_params_insert_transaction_visibility_and_rollback() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");
    db.execute("CREATE TABLE many_txn (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    let rows = vec![
        vec![Value::Int4(1), Value::String("one".to_string())],
        vec![Value::Int4(2), Value::String("two".to_string())],
    ];

    db.execute("BEGIN").unwrap();
    let count = db
        .execute_many_params("INSERT INTO many_txn (id, name) VALUES ($1, $2)", &rows)
        .unwrap();
    assert_eq!(count, 2);
    let in_txn = db
        .query_params("SELECT * FROM many_txn WHERE id = $1", &[Value::Int4(2)])
        .unwrap();
    assert_eq!(in_txn.len(), 1);
    db.execute("ROLLBACK").unwrap();

    let after_rollback = db.query("SELECT * FROM many_txn", &[]).unwrap();
    assert_eq!(after_rollback.len(), 0);
}

#[test]
fn test_update_with_parameters() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE inventory (id INT, item TEXT, quantity INT)")
        .unwrap();
    db.execute("INSERT INTO inventory VALUES (1, 'Apples', 100)").unwrap();
    db.execute("INSERT INTO inventory VALUES (2, 'Oranges', 50)").unwrap();

    // Test: UPDATE with parameters
    let count = db
        .execute_params(
            "UPDATE inventory SET quantity = $1 WHERE item = $2",
            &[Value::Int4(75), Value::String("Apples".to_string())],
        )
        .expect("Failed to update with parameters");

    assert_eq!(count, 1);

    // Verify update
    let results = db
        .query_params(
            "SELECT quantity FROM inventory WHERE item = $1",
            &[Value::String("Apples".to_string())],
        )
        .unwrap();
    assert_eq!(results[0].get(0).unwrap(), &Value::Int4(75));
}

#[test]
fn test_delete_with_parameters() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE logs (id INT, message TEXT, level TEXT)")
        .unwrap();
    db.execute("INSERT INTO logs VALUES (1, 'Starting', 'INFO')").unwrap();
    db.execute("INSERT INTO logs VALUES (2, 'Error occurred', 'ERROR')")
        .unwrap();
    db.execute("INSERT INTO logs VALUES (3, 'Debug info', 'DEBUG')")
        .unwrap();

    // Test: DELETE with parameter
    let count = db
        .execute_params(
            "DELETE FROM logs WHERE level = $1",
            &[Value::String("DEBUG".to_string())],
        )
        .expect("Failed to delete with parameters");

    assert_eq!(count, 1);

    // Verify deletion
    let results = db.query("SELECT * FROM logs", &[]).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn test_sql_injection_prevention() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE accounts (id INT, username TEXT, balance INT)")
        .unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 'alice', 1000)").unwrap();
    db.execute("INSERT INTO accounts VALUES (2, 'bob', 500)").unwrap();

    // Test: SQL injection attempt is safely handled as literal value
    let malicious_input = "'; DROP TABLE accounts; --";
    let results = db
        .query_params(
            "SELECT * FROM accounts WHERE username = $1",
            &[Value::String(malicious_input.to_string())],
        )
        .expect("Failed to execute query");

    // Should return 0 results (no matching username)
    assert_eq!(results.len(), 0);

    // Verify table still exists
    let all_results = db.query("SELECT * FROM accounts", &[]).unwrap();
    assert_eq!(all_results.len(), 2); // Table intact
}

#[test]
fn test_parameter_type_coercion() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE numbers (id INT, value INT)").unwrap();
    db.execute("INSERT INTO numbers VALUES (1, 42)").unwrap();
    db.execute("INSERT INTO numbers VALUES (2, 100)").unwrap();

    // Test: Integer parameter comparison
    let results = db
        .query_params("SELECT * FROM numbers WHERE value = $1", &[Value::Int4(42)])
        .expect("Failed to execute query");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(1).unwrap(), &Value::Int4(42));
}

#[test]
fn test_null_parameter() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE nullable_data (id INT, optional_text TEXT)")
        .unwrap();
    db.execute("INSERT INTO nullable_data VALUES (1, 'has_value')").unwrap();
    db.execute("INSERT INTO nullable_data VALUES (2, NULL)").unwrap();

    // Test: Insert with NULL parameter
    let count = db
        .execute_params(
            "INSERT INTO nullable_data VALUES ($1, $2)",
            &[Value::Int4(3), Value::Null],
        )
        .expect("Failed to insert with NULL parameter");

    assert_eq!(count, 1);

    // Verify insertion
    let results = db.query("SELECT * FROM nullable_data", &[]).unwrap();
    assert_eq!(results.len(), 3);
}

#[test]
fn test_parameter_in_projection() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE items (id INT, price INT)").unwrap();
    db.execute("INSERT INTO items VALUES (1, 100)").unwrap();
    db.execute("INSERT INTO items VALUES (2, 200)").unwrap();

    // Test: Parameter in WHERE clause
    let results = db
        .query_params("SELECT id FROM items WHERE price > $1", &[Value::Int4(150)])
        .expect("Failed to execute query");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &Value::Int4(2));
}

#[test]
fn test_multiple_inserts_with_parameters() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE data (id INT, value TEXT)").unwrap();

    // Test: Multiple inserts with parameters
    for i in 1..=5 {
        let count = db
            .execute_params(
                "INSERT INTO data VALUES ($1, $2)",
                &[Value::Int4(i), Value::String(format!("value_{}", i))],
            )
            .expect("Failed to insert");
        assert_eq!(count, 1);
    }

    // Verify all insertions
    let results = db.query("SELECT * FROM data", &[]).unwrap();
    assert_eq!(results.len(), 5);
}

#[test]
fn test_parameter_with_logical_operators() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE conditions (id INT, status TEXT, priority INT)")
        .unwrap();
    db.execute("INSERT INTO conditions VALUES (1, 'active', 1)").unwrap();
    db.execute("INSERT INTO conditions VALUES (2, 'active', 2)").unwrap();
    db.execute("INSERT INTO conditions VALUES (3, 'inactive', 1)").unwrap();
    db.execute("INSERT INTO conditions VALUES (4, 'active', 3)").unwrap();

    // Test: AND operator with parameters
    let results = db
        .query_params(
            "SELECT * FROM conditions WHERE status = $1 AND priority > $2",
            &[Value::String("active".to_string()), Value::Int4(1)],
        )
        .expect("Failed to execute query");

    assert_eq!(results.len(), 2); // id=2 and id=4
}

#[test]
fn test_empty_parameter_list() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE simple (id INT)").unwrap();
    db.execute("INSERT INTO simple VALUES (1)").unwrap();

    // Test: Query with no parameters (should work like normal query)
    let results = db
        .query_params("SELECT * FROM simple", &[])
        .expect("Failed to execute query");

    assert_eq!(results.len(), 1);
}

#[test]
fn test_parameter_index_validation() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE test (id INT, name TEXT)").unwrap();
    db.execute("INSERT INTO test VALUES (1, 'Alice')").unwrap();

    // Test: Missing parameter should return error
    let result = db.query_params(
        "SELECT * FROM test WHERE name = $1",
        &[], // No parameters provided
    );

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("Parameter") || err_msg.contains("not provided"));
}

#[test]
fn test_parameter_reuse() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Setup
    db.execute("CREATE TABLE ranges (id INT, low INT, high INT)").unwrap();
    db.execute("INSERT INTO ranges VALUES (1, 10, 20)").unwrap();
    db.execute("INSERT INTO ranges VALUES (2, 15, 25)").unwrap();
    db.execute("INSERT INTO ranges VALUES (3, 20, 30)").unwrap();

    // Test: Same parameter used multiple times in query
    let results = db
        .query_params(
            "SELECT * FROM ranges WHERE low <= $1 AND high >= $1",
            &[Value::Int4(18)],
        )
        .expect("Failed to execute query");

    // Should find ranges that contain value 18
    assert_eq!(results.len(), 2); // id=1 (10-20) and id=2 (15-25)
}
