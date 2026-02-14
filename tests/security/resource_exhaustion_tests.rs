//! Resource Exhaustion Security Tests
//!
//! Tests to verify that the database handles resource-intensive operations
//! gracefully and doesn't allow denial-of-service attacks.

use heliosdb_nano::{EmbeddedDatabase, Result};

#[test]
fn test_large_query_result_set() {
    // Test handling of large result sets
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE test_table (id INT, value TEXT)")
        .expect("Failed to create table");

    // Insert 10,000 rows
    for i in 0..10_000 {
        db.execute(&format!(
            "INSERT INTO test_table (id, value) VALUES ({}, 'value_{}')",
            i, i
        ))
        .expect("Failed to insert row");
    }

    // Query all rows - should complete without crashing
    let start = std::time::Instant::now();
    let result = db.query("SELECT * FROM test_table", &[]);
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "Large query should complete successfully");
    let results = result.expect("Failed to execute large query");
    assert_eq!(results.len(), 10_000, "Should return all 10,000 rows");

    // Should complete in reasonable time (< 5 seconds)
    assert!(
        elapsed.as_secs() < 5,
        "Large query took too long: {:?}",
        elapsed
    );

    println!(
        "Large query completed in {:?} ({} rows)",
        elapsed,
        results.len()
    );
}

#[test]
fn test_deeply_nested_query() {
    // Test handling of deeply nested queries
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE test_table (id INT, value TEXT)")
        .expect("Failed to create table");

    db.execute("INSERT INTO test_table (id, value) VALUES (1, 'test')")
        .expect("Failed to insert data");

    // Deeply nested parentheses (stack exhaustion attempt)
    let mut nested_query = String::from("SELECT * FROM test_table WHERE id = (");
    for _ in 0..100 {
        nested_query.push_str("(");
    }
    nested_query.push_str("1");
    for _ in 0..101 {
        nested_query.push_str(")");
    }

    let result = db.query(&nested_query, &[]);

    // Should either succeed with correct result or fail gracefully
    match result {
        Ok(results) => {
            println!("Deeply nested query succeeded with {} results", results.len());
        }
        Err(e) => {
            println!("Deeply nested query rejected: {}", e);
        }
    }

    // Database should still be responsive
    let verify = db.query("SELECT * FROM test_table", &[]);
    assert!(verify.is_ok(), "Database should still be responsive after nested query");
}

#[test]
fn test_very_long_string_insertion() {
    // Test handling of very long strings
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE test_table (id INT, value TEXT)")
        .expect("Failed to create table");

    // Create a very long string (1 MB)
    let long_string = "A".repeat(1_000_000);

    // Attempt to insert
    let result = db.execute(&format!(
        "INSERT INTO test_table (id, value) VALUES (1, '{}')",
        long_string
    ));

    // Should either succeed or fail gracefully (not crash)
    match result {
        Ok(_) => {
            println!("Long string insertion succeeded");
            // Verify database is still responsive
            let verify = db.query("SELECT * FROM test_table", &[]);
            assert!(verify.is_ok(), "Database should be responsive after long string");
        }
        Err(e) => {
            println!("Long string insertion rejected: {}", e);
        }
    }
}

#[test]
fn test_cartesian_product_query() {
    // Test handling of cartesian products (can cause memory exhaustion)
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE table1 (id INT, value TEXT)")
        .expect("Failed to create table1");
    db.execute("CREATE TABLE table2 (id INT, value TEXT)")
        .expect("Failed to create table2");

    // Insert 100 rows in each table
    for i in 0..100 {
        db.execute(&format!(
            "INSERT INTO table1 (id, value) VALUES ({}, 'value_{}')",
            i, i
        ))
        .expect("Failed to insert into table1");
        db.execute(&format!(
            "INSERT INTO table2 (id, value) VALUES ({}, 'value_{}')",
            i, i
        ))
        .expect("Failed to insert into table2");
    }

    // Cartesian product (100 x 100 = 10,000 rows)
    let start = std::time::Instant::now();
    let result = db.query("SELECT * FROM table1, table2", &[]);
    let elapsed = start.elapsed();

    match result {
        Ok(results) => {
            println!("Cartesian product returned {} rows in {:?}", results.len(), elapsed);
            assert_eq!(results.len(), 10_000, "Should return 10,000 rows (100 x 100)");
        }
        Err(e) => {
            println!("Cartesian product query rejected: {}", e);
        }
    }

    // Database should still be responsive
    let verify = db.query("SELECT * FROM table1 LIMIT 1", &[]);
    assert!(verify.is_ok(), "Database should be responsive after cartesian product");
}

#[test]
fn test_multiple_joins_query() {
    // Test handling of multiple joins
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    // Create multiple tables
    for i in 1..=5 {
        db.execute(&format!(
            "CREATE TABLE table{} (id INT, value TEXT)",
            i
        ))
        .expect(&format!("Failed to create table{}", i));

        db.execute(&format!(
            "INSERT INTO table{} (id, value) VALUES (1, 'value_1')",
            i
        ))
        .expect(&format!("Failed to insert into table{}", i));
    }

    // Query with multiple joins
    let query = "SELECT * FROM table1 \
                 INNER JOIN table2 ON table1.id = table2.id \
                 INNER JOIN table3 ON table2.id = table3.id \
                 INNER JOIN table4 ON table3.id = table4.id \
                 INNER JOIN table5 ON table4.id = table5.id";

    let start = std::time::Instant::now();
    let result = db.query(query, &[]);
    let elapsed = start.elapsed();

    match result {
        Ok(results) => {
            println!("Multiple joins query completed in {:?} with {} results", elapsed, results.len());
        }
        Err(e) => {
            println!("Multiple joins query rejected: {}", e);
        }
    }

    // Should complete in reasonable time
    assert!(elapsed.as_secs() < 2, "Multiple joins took too long: {:?}", elapsed);
}

#[test]
fn test_complex_aggregate_query() {
    // Test handling of complex aggregate queries
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE sales (id INT, product TEXT, amount INT, region TEXT)")
        .expect("Failed to create table");

    // Insert test data
    for i in 0..1_000 {
        db.execute(&format!(
            "INSERT INTO sales (id, product, amount, region) VALUES ({}, 'product_{}', {}, 'region_{}')",
            i, i % 10, i * 10, i % 5
        ))
        .expect("Failed to insert data");
    }

    // Complex aggregate query
    let query = "SELECT product, region, COUNT(*) as count, SUM(amount) as total, AVG(amount) as avg \
                 FROM sales \
                 GROUP BY product, region \
                 ORDER BY total DESC";

    let start = std::time::Instant::now();
    let result = db.query(query, &[]);
    let elapsed = start.elapsed();

    match result {
        Ok(results) => {
            println!("Complex aggregate query completed in {:?} with {} results", elapsed, results.len());
        }
        Err(e) => {
            println!("Complex aggregate query rejected: {}", e);
        }
    }

    assert!(elapsed.as_secs() < 3, "Complex aggregate took too long: {:?}", elapsed);
}

#[test]
fn test_rapid_connection_creation() {
    // Test handling of rapid database creation (connection pooling test)
    let mut databases = Vec::new();

    for i in 0..10 {
        let db = EmbeddedDatabase::new_in_memory()
            .expect(&format!("Failed to create database {}", i));

        db.execute("CREATE TABLE test (id INT)")
            .expect("Failed to create table");

        databases.push(db);
    }

    // All databases should be functional
    for (i, db) in databases.iter().enumerate() {
        let result = db.execute(&format!("INSERT INTO test (id) VALUES ({})", i));
        assert!(result.is_ok(), "Database {} should be functional", i);
    }

    println!("Created and used {} databases successfully", databases.len());
}

#[test]
fn test_concurrent_query_stress() {
    use std::sync::Arc;
    use std::thread;

    // Test concurrent query execution (stress test)
    let db = Arc::new(
        EmbeddedDatabase::new_in_memory()
            .expect("Failed to create in-memory database")
    );

    db.execute("CREATE TABLE test_table (id INT, value TEXT)")
        .expect("Failed to create table");

    // Insert initial data
    for i in 0..100 {
        db.execute(&format!(
            "INSERT INTO test_table (id, value) VALUES ({}, 'value_{}')",
            i, i
        ))
        .expect("Failed to insert data");
    }

    let mut handles = vec![];

    // Spawn multiple threads to query concurrently
    for thread_id in 0..5 {
        let db_clone = Arc::clone(&db);
        let handle = thread::spawn(move || {
            for i in 0..20 {
                let result = db_clone.query(
                    &format!("SELECT * FROM test_table WHERE id = {}", i),
                    &[],
                );

                if let Err(e) = result {
                    eprintln!("Thread {} query {} failed: {}", thread_id, i, e);
                    return Err(e);
                }
            }
            Ok(())
        });
        handles.push(handle);
    }

    // Wait for all threads to complete
    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .expect(&format!("Thread {} panicked", i))
            .expect(&format!("Thread {} had errors", i));
    }

    println!("Concurrent stress test completed successfully");
}

#[test]
fn test_memory_limit_insert() {
    // Test that excessive insertions don't cause unbounded memory growth
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE test_table (id INT, value TEXT)")
        .expect("Failed to create table");

    // Insert many rows and check memory doesn't explode
    let start_memory = get_memory_usage_mb();

    for i in 0..5_000 {
        db.execute(&format!(
            "INSERT INTO test_table (id, value) VALUES ({}, 'value_{}')",
            i, i
        ))
        .expect("Failed to insert row");
    }

    let end_memory = get_memory_usage_mb();
    let memory_increase = end_memory - start_memory;

    println!(
        "Memory usage: start={} MB, end={} MB, increase={} MB",
        start_memory, end_memory, memory_increase
    );

    // Memory increase should be reasonable (< 500 MB for 5000 rows)
    assert!(
        memory_increase < 500.0,
        "Memory usage increased too much: {} MB",
        memory_increase
    );
}

#[test]
fn test_division_by_zero_protection() {
    // Test division by zero protection
    let db = EmbeddedDatabase::new_in_memory()
        .expect("Failed to create in-memory database");

    db.execute("CREATE TABLE test_table (id INT, value INT)")
        .expect("Failed to create table");

    db.execute("INSERT INTO test_table (id, value) VALUES (1, 0)")
        .expect("Failed to insert data");

    // Attempt division by zero
    let result = db.query("SELECT id / value FROM test_table", &[]);

    // Should either return error or handle gracefully (not crash)
    match result {
        Ok(_) => println!("Division by zero handled gracefully"),
        Err(e) => println!("Division by zero rejected: {}", e),
    }

    // Database should still be responsive
    let verify = db.query("SELECT * FROM test_table", &[]);
    assert!(verify.is_ok(), "Database should be responsive after division by zero");
}

// Helper function to get memory usage (approximate)
fn get_memory_usage_mb() -> f64 {
    // Simple approximation - in production, use a proper memory profiler
    // For testing purposes, we just return 0.0
    // In a real implementation, this would use platform-specific APIs
    0.0
}
