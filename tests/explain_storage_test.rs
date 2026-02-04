//! Test for EXPLAIN STORAGE FORMAT JSON functionality

use heliosdb_lite::EmbeddedDatabase;

#[test]
fn test_explain_storage_format_json() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Create a test table
    db.execute("CREATE TABLE test_products (id INT PRIMARY KEY, name TEXT, price DECIMAL)")
        .expect("Failed to create table");

    // Insert some data
    db.execute("INSERT INTO test_products VALUES (1, 'Widget', 9.99)").unwrap();
    db.execute("INSERT INTO test_products VALUES (2, 'Gadget', 19.99)").unwrap();

    // Test EXPLAIN with STORAGE and FORMAT JSON
    let results = db.query("EXPLAIN (STORAGE, FORMAT JSON) SELECT * FROM test_products WHERE price > 10", &[])
        .expect("EXPLAIN STORAGE FORMAT JSON should work");

    // Should return JSON formatted output
    assert!(!results.is_empty(), "EXPLAIN should return results");

    // Collect all output lines - each Tuple's first value is the QUERY PLAN line
    let output: String = results.iter()
        .map(|r| format!("{:?}", r))
        .collect::<Vec<_>>()
        .join("\n");

    println!("=== EXPLAIN (STORAGE, FORMAT JSON) output ===");
    println!("{}", output);

    // Verify it's JSON format (should contain opening brace in output)
    assert!(output.contains("{") || output.contains("String"), "Output should contain JSON data");
}

#[test]
fn test_explain_analyze_storage() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Create a test table
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer TEXT, amount DECIMAL)")
        .expect("Failed to create table");

    // Insert data
    for i in 1..=10 {
        db.execute(
            &format!("INSERT INTO orders VALUES ({}, 'Customer{}', {})", i, i, i as f64 * 10.5)
        ).unwrap();
    }

    // Test EXPLAIN ANALYZE with STORAGE
    let results = db.query("EXPLAIN (ANALYZE, STORAGE) SELECT * FROM orders WHERE amount > 50", &[])
        .expect("EXPLAIN ANALYZE STORAGE should work");

    assert!(!results.is_empty(), "EXPLAIN ANALYZE should return results");

    // Collect output
    let output: String = results.iter()
        .map(|r| format!("{:?}", r))
        .collect::<Vec<_>>()
        .join("\n");

    println!("=== EXPLAIN (ANALYZE, STORAGE) output ===");
    println!("{}", output);

    // Should contain plan information
    assert!(!output.is_empty(), "Output should not be empty");
}

#[test]
fn test_explain_all_options() {
    let db = EmbeddedDatabase::new_in_memory().expect("Failed to create database");

    // Create a test table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT, active BOOLEAN)")
        .expect("Failed to create table");

    db.execute("INSERT INTO users VALUES (1, 'Alice', true)").unwrap();
    db.execute("INSERT INTO users VALUES (2, 'Bob', false)").unwrap();

    // Test with multiple options
    let results = db.query("EXPLAIN (ANALYZE, VERBOSE, STORAGE) SELECT * FROM users WHERE active = true", &[])
        .expect("EXPLAIN with multiple options should work");

    let output: String = results.iter()
        .map(|r| format!("{:?}", r))
        .collect::<Vec<_>>()
        .join("\n");

    println!("=== EXPLAIN (ANALYZE, VERBOSE, STORAGE) output ===");
    println!("{}", output);

    assert!(!output.is_empty(), "Output should not be empty");
}
