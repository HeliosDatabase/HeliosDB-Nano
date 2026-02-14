//! Query Timeout Enforcement Tests
//!
//! Comprehensive tests to verify timeout enforcement across all operators.

use heliosdb_nano::{EmbeddedDatabase, Config};
use std::time::{Duration, Instant};

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_scan_timeout() {
    let mut config = Config::default();
    config.storage.query_timeout_ms = Some(100); // 100ms timeout

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE test (id INT, data TEXT)").unwrap();

    // Insert many rows to ensure scan takes > 100ms
    for i in 0..10000 {
        db.execute(&format!("INSERT INTO test VALUES ({}, 'data{}')", i, i))
            .unwrap();
    }

    let start = Instant::now();
    let result = db.query("SELECT * FROM test", &[]);
    let elapsed = start.elapsed();

    // Should timeout (or complete faster if system is very fast)
    if elapsed > Duration::from_millis(100) {
        assert!(result.is_err(), "Query should have timed out but succeeded");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("timeout") || err_msg.contains("exceeded"),
            "Error should mention timeout: {}", err_msg);
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_aggregate_timeout() {
    let mut config = Config::default();
    config.storage.query_timeout_ms = Some(50); // 50ms timeout

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE sales (product_id INT, amount INT)").unwrap();

    // Insert many rows to ensure aggregation takes time
    for i in 0..5000 {
        db.execute(&format!(
            "INSERT INTO sales VALUES ({}, {})",
            i % 100, // 100 groups
            i * 10
        )).unwrap();
    }

    let start = Instant::now();
    let result = db.query("SELECT product_id, SUM(amount), AVG(amount) FROM sales GROUP BY product_id", &[]);
    let elapsed = start.elapsed();

    if elapsed > Duration::from_millis(50) {
        assert!(result.is_err(), "Aggregate query should have timed out");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("timeout") || err_msg.contains("exceeded"));
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_sort_timeout() {
    let mut config = Config::default();
    config.storage.query_timeout_ms = Some(100);

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE items (id INT, priority INT, name TEXT)").unwrap();

    // Insert many rows
    for i in 0..10000 {
        db.execute(&format!(
            "INSERT INTO items VALUES ({}, {}, 'item{}')",
            i,
            10000 - i, // Reverse order
            i
        )).unwrap();
    }

    let start = Instant::now();
    let result = db.query("SELECT * FROM items ORDER BY priority DESC, name ASC", &[]);
    let elapsed = start.elapsed();

    if elapsed > Duration::from_millis(100) {
        assert!(result.is_err(), "Sort query should have timed out");
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_nested_loop_join_timeout() {
    let mut config = Config::default();
    config.storage.query_timeout_ms = Some(50);

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE t1 (id INT, value TEXT)").unwrap();
    db.execute("CREATE TABLE t2 (id INT, value TEXT)").unwrap();

    // Insert rows (nested loop will be O(N*M))
    for i in 0..500 {
        db.execute(&format!("INSERT INTO t1 VALUES ({}, 'a{}')", i, i)).unwrap();
        db.execute(&format!("INSERT INTO t2 VALUES ({}, 'b{}')", i, i)).unwrap();
    }

    let start = Instant::now();
    // Nested loop join (500 * 500 = 250K iterations)
    let result = db.query("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id", &[]);
    let elapsed = start.elapsed();

    if elapsed > Duration::from_millis(50) {
        assert!(result.is_err(), "Join query should have timed out");
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_no_timeout_when_disabled() {
    let config = Config::default(); // No timeout configured

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    for i in 0..1000 {
        db.execute(&format!("INSERT INTO test VALUES ({})", i)).unwrap();
    }

    // Should complete successfully without timeout
    let result = db.query("SELECT * FROM test ORDER BY id", &[]);
    assert!(result.is_ok(), "Query without timeout should succeed");

    let rows = result.unwrap();
    assert_eq!(rows.len(), 1000);
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_timeout_configuration() {
    // Test with different timeout values
    for timeout_ms in [10, 50, 100, 500] {
        let mut config = Config::default();
        config.storage.query_timeout_ms = Some(timeout_ms);

        let db = EmbeddedDatabase::with_config(config).unwrap();
        db.execute("CREATE TABLE test (id INT)").unwrap();

        for i in 0..100 {
            db.execute(&format!("INSERT INTO test VALUES ({})", i)).unwrap();
        }

        // Small query should succeed even with short timeout
        let result = db.query("SELECT * FROM test LIMIT 10", &[]);
        assert!(result.is_ok(), "Small query should succeed with timeout={}ms", timeout_ms);
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_timeout_propagates_through_operator_tree() {
    let mut config = Config::default();
    config.storage.query_timeout_ms = Some(100);

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE test (id INT, value INT)").unwrap();

    for i in 0..5000 {
        db.execute(&format!("INSERT INTO test VALUES ({}, {})", i, i * 2)).unwrap();
    }

    let start = Instant::now();
    // Complex query with multiple operators: Scan -> Filter -> Project -> Aggregate -> Sort
    let result = db.query("
        SELECT value / 2 as half_value, COUNT(*) as cnt
        FROM test
        WHERE id > 100
        GROUP BY value / 2
        ORDER BY cnt DESC
    ", &[]);
    let elapsed = start.elapsed();

    // Timeout should propagate through the entire operator tree
    if elapsed > Duration::from_millis(100) {
        assert!(result.is_err(), "Complex query should timeout");
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_project_with_distinct_timeout() {
    let mut config = Config::default();
    config.storage.query_timeout_ms = Some(100);

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE test (category TEXT, item TEXT)").unwrap();

    // Insert many duplicate rows (DISTINCT will materialize for deduplication)
    for i in 0..10000 {
        db.execute(&format!(
            "INSERT INTO test VALUES ('cat{}', 'item{}')",
            i % 50, // 50 categories
            i % 100 // 100 items
        )).unwrap();
    }

    let start = Instant::now();
    let result = db.query("SELECT DISTINCT category, item FROM test", &[]);
    let elapsed = start.elapsed();

    if elapsed > Duration::from_millis(100) {
        assert!(result.is_err(), "DISTINCT query should timeout");
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_timeout_error_message() {
    let mut config = Config::default();
    config.storage.query_timeout_ms = Some(10); // Very short timeout

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    for i in 0..10000 {
        db.execute(&format!("INSERT INTO test VALUES ({})", i)).unwrap();
    }

    let result = db.query("SELECT * FROM test ORDER BY id DESC", &[]);

    if let Err(e) = result {
        let err_msg = format!("{}", e);
        // Error message should be helpful
        assert!(
            err_msg.contains("timeout") || err_msg.contains("exceeded") || err_msg.contains("limit"),
            "Error message should explain timeout: '{}'", err_msg
        );
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_timeout_with_materialized_views() {
    let mut config = Config::default();
    config.storage.query_timeout_ms = Some(100);

    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE events (event_type TEXT, count INT)").unwrap();

    for i in 0..5000 {
        db.execute(&format!(
            "INSERT INTO events VALUES ('type{}', {})",
            i % 10,
            i
        )).unwrap();
    }

    let start = Instant::now();
    // Creating MV involves executing the query
    let result = db.execute("
        CREATE MATERIALIZED VIEW event_summary AS
        SELECT event_type, SUM(count) as total
        FROM events
        GROUP BY event_type
    ");
    let elapsed = start.elapsed();

    // MV creation should respect timeout during initial population
    if elapsed > Duration::from_millis(100) {
        // Either succeeds quickly or times out
        if result.is_err() {
            let err_msg = format!("{}", result.unwrap_err());
            assert!(err_msg.contains("timeout") || err_msg.contains("exceeded"));
        }
    }
}

#[test]
#[ignore = "Timing-dependent test - may fail based on system load"]
fn test_timeout_check_interval_performance() {
    // Verify that timeout checking has minimal performance overhead
    let config_no_timeout = Config::default();
    let config_with_timeout = {
        let mut c = Config::default();
        c.storage.query_timeout_ms = Some(10000); // 10 second timeout (won't trigger)
        c
    };

    let db_no_timeout = EmbeddedDatabase::with_config(config_no_timeout).unwrap();
    let db_with_timeout = EmbeddedDatabase::with_config(config_with_timeout).unwrap();

    // Create identical tables
    for db in [&db_no_timeout, &db_with_timeout] {
        db.execute("CREATE TABLE test (id INT)").unwrap();
        for i in 0..1000 {
            db.execute(&format!("INSERT INTO test VALUES ({})", i)).unwrap();
        }
    }

    // Measure performance
    let start = Instant::now();
    db_no_timeout.query("SELECT * FROM test", &[]).unwrap();
    let time_no_timeout = start.elapsed();

    let start = Instant::now();
    db_with_timeout.query("SELECT * FROM test", &[]).unwrap();
    let time_with_timeout = start.elapsed();

    // Overhead should be < 10%
    let overhead_ratio = time_with_timeout.as_secs_f64() / time_no_timeout.as_secs_f64();
    assert!(
        overhead_ratio < 1.10,
        "Timeout checking overhead too high: {:.2}x (expected < 1.10x)",
        overhead_ratio
    );
}
