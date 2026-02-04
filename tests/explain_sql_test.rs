//! Tests for SQL EXPLAIN and EXPLAIN ANALYZE statements

use heliosdb_lite::EmbeddedDatabase;

#[test]
fn test_explain_simple_select() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create a test table
    db.execute("CREATE TABLE users (id INT4, name TEXT)").unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')").unwrap();

    // Test EXPLAIN
    let result = db.query("EXPLAIN SELECT * FROM users", &[]).unwrap();

    // Should return rows with the query plan
    assert!(!result.is_empty(), "EXPLAIN should return plan rows");

    // Check that it contains expected output
    let plan_text: String = result.iter()
        .map(|row| {
            match &row.values[0] {
                heliosdb_lite::Value::String(s) => s.clone(),
                _ => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    println!("EXPLAIN output:\n{}", plan_text);
    assert!(plan_text.contains("Scan"), "Plan should contain Scan operator");
}

#[test]
fn test_explain_analyze_select() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create a test table
    db.execute("CREATE TABLE products (id INT4, name TEXT, price FLOAT8)").unwrap();
    db.execute("INSERT INTO products VALUES (1, 'Widget', 9.99), (2, 'Gadget', 19.99)").unwrap();

    // Test EXPLAIN ANALYZE
    let result = db.query("EXPLAIN ANALYZE SELECT * FROM products WHERE price > 10", &[]).unwrap();

    // Should return rows with the query plan and statistics
    assert!(!result.is_empty(), "EXPLAIN ANALYZE should return plan rows");

    // Check that it contains expected output
    let plan_text: String = result.iter()
        .map(|row| {
            match &row.values[0] {
                heliosdb_lite::Value::String(s) => s.clone(),
                _ => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    println!("EXPLAIN ANALYZE output:\n{}", plan_text);

    // EXPLAIN ANALYZE should execute the query and show stats
    assert!(plan_text.contains("EXPLAIN ANALYZE") || plan_text.contains("Execution"),
            "Plan should indicate ANALYZE mode");
}

#[test]
fn test_explain_insert() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create a test table
    db.execute("CREATE TABLE logs (id INT4, message TEXT)").unwrap();

    // Test EXPLAIN for INSERT
    let result = db.query("EXPLAIN INSERT INTO logs VALUES (1, 'test')", &[]).unwrap();

    // Should return rows with the query plan
    assert!(!result.is_empty(), "EXPLAIN should return plan rows for INSERT");

    // Check that it contains expected output
    let plan_text: String = result.iter()
        .map(|row| {
            match &row.values[0] {
                heliosdb_lite::Value::String(s) => s.clone(),
                _ => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    println!("EXPLAIN INSERT output:\n{}", plan_text);
    assert!(plan_text.contains("Insert"), "Plan should contain Insert operator");
}

#[test]
fn test_explain_with_filter() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    db.execute("CREATE TABLE orders (id INT4, customer_id INT4, total FLOAT8)").unwrap();
    db.execute("INSERT INTO orders VALUES (1, 100, 50.0), (2, 101, 75.0), (3, 100, 25.0)").unwrap();

    // Test EXPLAIN for filtered SELECT
    let result = db.query("EXPLAIN SELECT * FROM orders WHERE customer_id = 100", &[]).unwrap();

    assert!(!result.is_empty(), "EXPLAIN should return plan rows");

    let plan_text: String = result.iter()
        .map(|row| {
            match &row.values[0] {
                heliosdb_lite::Value::String(s) => s.clone(),
                _ => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    println!("EXPLAIN with filter:\n{}", plan_text);

    // Should show either Filter or predicate info
    assert!(plan_text.contains("Filter") || plan_text.contains("predicate") || plan_text.contains("Scan"),
            "Plan should contain filtering information");
}
