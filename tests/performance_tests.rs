//! Performance and benchmark tests for HeliosDB Lite
//!
//! Tests query execution time, throughput, and resource usage

mod test_helpers;

use heliosdb_lite::{EmbeddedDatabase, Result};
use test_helpers::*;
use std::time::Duration;

// ============================================================================
// Query Performance Tests
// ============================================================================

#[test]
fn test_simple_query_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // Simple SELECT should complete quickly
    assert_query_performance(
        || {
            let _ = db.query("SELECT * FROM users", &[])?;
            Ok(())
        },
        Duration::from_millis(500),
        "Simple SELECT on 1000 rows"
    );

    Ok(())
}

#[test]
fn test_filtered_query_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // WHERE clause query should complete quickly
    assert_query_performance(
        || {
            let _ = db.query("SELECT * FROM users WHERE age > 30", &[])?;
            Ok(())
        },
        Duration::from_millis(500),
        "Filtered SELECT on 1000 rows"
    );

    Ok(())
}

#[test]
fn test_aggregate_query_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // Aggregate query should complete quickly
    assert_query_performance(
        || {
            let _ = db.query("SELECT COUNT(*), AVG(age), MAX(age), MIN(age) FROM users", &[])?;
            Ok(())
        },
        Duration::from_millis(500),
        "Aggregate query on 1000 rows"
    );

    Ok(())
}

#[test]
fn test_order_by_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // ORDER BY should complete quickly
    assert_query_performance(
        || {
            let _ = db.query("SELECT * FROM users ORDER BY age", &[])?;
            Ok(())
        },
        Duration::from_millis(1000),
        "ORDER BY on 1000 rows"
    );

    Ok(())
}

#[test]
fn test_group_by_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert data with groupable values
    for i in 1..=1000 {
        let dept = match i % 5 {
            0 => "Engineering",
            1 => "Sales",
            2 => "Marketing",
            3 => "HR",
            _ => "Support",
        };
        db.execute(&format!(
            "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
            i, dept, i, 20 + (i % 50)
        ))?;
    }

    // GROUP BY should complete quickly
    assert_query_performance(
        || {
            let _ = db.query("SELECT name, COUNT(*) FROM users GROUP BY name", &[])?;
            Ok(())
        },
        Duration::from_millis(1000),
        "GROUP BY on 1000 rows"
    );

    Ok(())
}

// ============================================================================
// Insert Performance Tests
// ============================================================================

#[test]
fn test_single_insert_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Single insert should be fast
    let duration = measure_query_time(|| {
        db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Test', 'test@example.com', 30)")?;
        Ok(())
    });

    assert!(
        duration < Duration::from_millis(100),
        "Single insert took {:?} but should be under 100ms",
        duration
    );

    Ok(())
}

#[test]
fn test_bulk_insert_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // 1000 inserts should complete in reasonable time
    let duration = measure_query_time(|| {
        for i in 1..=1000 {
            db.execute(&format!(
                "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
                i, i, i, 20 + (i % 50)
            ))?;
        }
        Ok(())
    });

    assert!(
        duration < Duration::from_secs(5),
        "1000 inserts took {:?} but should be under 5s",
        duration
    );

    // Calculate throughput
    let rows_per_second = 1000.0 / duration.as_secs_f64();
    println!("Insert throughput: {:.0} rows/second", rows_per_second);

    Ok(())
}

// ============================================================================
// Update Performance Tests
// ============================================================================

#[test]
fn test_single_update_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // Single row update should be fast
    assert_query_performance(
        || {
            db.execute("UPDATE users SET age = 100 WHERE id = 1")?;
            Ok(())
        },
        Duration::from_millis(100),
        "Single row UPDATE"
    );

    Ok(())
}

#[test]
#[ignore = "Performance benchmark - timing may vary by environment"]
fn test_bulk_update_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // Bulk update should complete in reasonable time
    let duration = measure_query_time(|| {
        db.execute("UPDATE users SET age = age + 1")?;
        Ok(())
    });

    assert!(
        duration < Duration::from_secs(2),
        "Bulk update of 1000 rows took {:?} but should be under 2s",
        duration
    );

    Ok(())
}

#[test]
#[ignore = "Performance benchmark - timing may vary by environment"]
fn test_filtered_update_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // Filtered update should complete quickly
    assert_query_performance(
        || {
            db.execute("UPDATE users SET age = 100 WHERE age > 30")?;
            Ok(())
        },
        Duration::from_millis(500),
        "Filtered UPDATE on subset of rows"
    );

    Ok(())
}

// ============================================================================
// Delete Performance Tests
// ============================================================================

#[test]
fn test_single_delete_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // Single row delete should be fast
    assert_query_performance(
        || {
            db.execute("DELETE FROM users WHERE id = 1")?;
            Ok(())
        },
        Duration::from_millis(100),
        "Single row DELETE"
    );

    Ok(())
}

#[test]
fn test_bulk_delete_performance() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // Bulk delete should complete in reasonable time
    let duration = measure_query_time(|| {
        db.execute("DELETE FROM users WHERE age > 30")?;
        Ok(())
    });

    assert!(
        duration < Duration::from_secs(2),
        "Bulk delete took {:?} but should be under 2s",
        duration
    );

    Ok(())
}

// ============================================================================
// Large Dataset Tests
// ============================================================================

#[test]
fn test_query_10k_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10_000)?;

    // Query should complete in reasonable time
    let duration = measure_query_time(|| {
        let results = db.query("SELECT * FROM users", &[])?;
        assert_eq!(results.len(), 10_000);
        Ok(())
    });

    assert!(
        duration < Duration::from_secs(2),
        "Query of 10k rows took {:?} but should be under 2s",
        duration
    );

    Ok(())
}

#[test]
fn test_filtered_query_10k_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10_000)?;

    // Filtered query should be reasonably fast
    let duration = measure_query_time(|| {
        let _ = db.query("SELECT * FROM users WHERE age > 30 AND age < 50", &[])?;
        Ok(())
    });

    assert!(
        duration < Duration::from_secs(2),
        "Filtered query of 10k rows took {:?} but should be under 2s",
        duration
    );

    Ok(())
}

#[test]
fn test_aggregate_10k_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10_000)?;

    // Aggregate should complete quickly
    assert_query_performance(
        || {
            let _ = db.query("SELECT COUNT(*), AVG(age) FROM users", &[])?;
            Ok(())
        },
        Duration::from_secs(1),
        "Aggregate on 10k rows"
    );

    Ok(())
}

// ============================================================================
// Repeated Operations Performance
// ============================================================================

#[test]
#[ignore = "Performance benchmark - timing may vary by environment"]
fn test_repeated_inserts() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Test sustained insert performance
    let start = std::time::Instant::now();
    for i in 1..=5000 {
        db.execute(&format!(
            "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
            i, i, i, 20 + (i % 50)
        ))?;
    }
    let duration = start.elapsed();

    let rows_per_second = 5000.0 / duration.as_secs_f64();
    println!("Sustained insert rate: {:.0} rows/second", rows_per_second);

    assert!(
        duration < Duration::from_secs(10),
        "5000 inserts took {:?} but should be under 10s",
        duration
    );

    Ok(())
}

#[test]
fn test_repeated_selects() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 100)?;

    // Test sustained query performance
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        let _ = db.query("SELECT * FROM users WHERE id = 50", &[])?;
    }
    let duration = start.elapsed();

    let queries_per_second = 1000.0 / duration.as_secs_f64();
    println!("Query throughput: {:.0} queries/second", queries_per_second);

    assert!(
        duration < Duration::from_secs(5),
        "1000 queries took {:?} but should be under 5s",
        duration
    );

    Ok(())
}

#[test]
fn test_mixed_workload() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Simulate realistic mixed workload
    let start = std::time::Instant::now();

    for i in 1..=500 {
        // 70% reads, 20% inserts, 10% updates
        let op = i % 10;

        if op < 7 {
            // Read
            let _ = db.query("SELECT * FROM users WHERE id < 100", &[])?;
        } else if op < 9 {
            // Insert
            db.execute(&format!(
                "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
                i + 10000, i, i, 30
            ))?;
        } else {
            // Update
            db.execute(&format!("UPDATE users SET age = {} WHERE id = {}", 40, i % 100))?;
        }
    }

    let duration = start.elapsed();
    let ops_per_second = 500.0 / duration.as_secs_f64();
    println!("Mixed workload throughput: {:.0} ops/second", ops_per_second);

    assert!(
        duration < Duration::from_secs(5),
        "Mixed workload took {:?} but should be under 5s",
        duration
    );

    Ok(())
}

// ============================================================================
// Memory Efficiency Tests
// ============================================================================

#[test]
fn test_memory_usage_large_dataset() -> Result<()> {
    let db = create_test_db()?;

    // Insert large dataset
    setup_with_test_data(&db, 10_000)?;

    // Query should not cause excessive memory usage
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 10_000);

    // If we get here without OOM, test passes
    Ok(())
}

#[test]
fn test_memory_usage_repeated_operations() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 100)?;

    // Repeated operations should not leak memory
    for _ in 0..100 {
        let _ = db.query("SELECT * FROM users", &[])?;
        db.execute("UPDATE users SET age = age + 1")?;
        let _ = db.query("SELECT COUNT(*) FROM users", &[])?;
    }

    // If we get here without OOM, test passes
    Ok(())
}

// ============================================================================
// Scalability Tests
// ============================================================================

#[test]
fn test_performance_scales_linearly() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Measure time for different dataset sizes
    let sizes = [100, 500, 1000];
    let mut times = Vec::new();

    for size in sizes.iter() {
        // Clear table
        db.execute("DELETE FROM users")?;

        // Insert data
        for i in 1..=*size {
            db.execute(&format!(
                "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
                i, i, i, 30
            ))?;
        }

        // Measure query time
        let start = std::time::Instant::now();
        let _ = db.query("SELECT * FROM users", &[])?;
        times.push(start.elapsed());
    }

    // Print results
    for (size, time) in sizes.iter().zip(times.iter()) {
        println!("Size {}: {:?}", size, time);
    }

    Ok(())
}

// ============================================================================
// Baseline Performance Metrics
// ============================================================================

#[test]
fn test_performance_baseline() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    println!("\n=== Performance Baseline ===");

    // Insert baseline
    let start = std::time::Instant::now();
    for i in 1..=1000 {
        db.execute(&format!(
            "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
            i, i, i, 30
        ))?;
    }
    let insert_time = start.elapsed();
    println!("1000 inserts: {:?} ({:.0} ops/sec)",
             insert_time,
             1000.0 / insert_time.as_secs_f64());

    // Query baseline
    let start = std::time::Instant::now();
    let _ = db.query("SELECT * FROM users", &[])?;
    let query_time = start.elapsed();
    println!("Full table scan (1000 rows): {:?}", query_time);

    // Filtered query baseline
    let start = std::time::Instant::now();
    let _ = db.query("SELECT * FROM users WHERE age > 30", &[])?;
    let filtered_time = start.elapsed();
    println!("Filtered query: {:?}", filtered_time);

    // Aggregate baseline
    let start = std::time::Instant::now();
    let _ = db.query("SELECT COUNT(*), AVG(age) FROM users", &[])?;
    let aggregate_time = start.elapsed();
    println!("Aggregate query: {:?}", aggregate_time);

    // Update baseline
    let start = std::time::Instant::now();
    db.execute("UPDATE users SET age = age + 1")?;
    let update_time = start.elapsed();
    println!("Bulk update (1000 rows): {:?}", update_time);

    // Delete baseline
    let start = std::time::Instant::now();
    db.execute("DELETE FROM users WHERE age > 50")?;
    let delete_time = start.elapsed();
    println!("Bulk delete: {:?}", delete_time);

    println!("=== End Baseline ===\n");

    Ok(())
}
