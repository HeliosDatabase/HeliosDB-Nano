//! Integration tests for Phase 3 SQL parsers with storage backends
//!
//! Tests the full execution path from SQL -> Parser -> Planner -> Executor -> Storage

use heliosdb_nano::{EmbeddedDatabase, Result};

/// Helper to execute SQL and get results using EmbeddedDatabase
/// This uses the full SQL execution path including Phase 3 extensions
fn execute_sql(db: &EmbeddedDatabase, sql: &str) -> Result<Vec<heliosdb_nano::Tuple>> {
    db.query(sql, &[])
}

/// Helper to execute SQL that doesn't return results (DDL, DML)
fn execute_ddl(db: &EmbeddedDatabase, sql: &str) -> Result<u64> {
    db.execute(sql)
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_branch_create_and_list() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create a branch
    execute_ddl(&db, "CREATE DATABASE BRANCH dev FROM CURRENT AS OF NOW").unwrap();

    // List branches using system view
    let results = execute_sql(&db, "SELECT * FROM pg_database_branches()").unwrap();

    // Should have at least 2 branches (main + dev)
    assert!(results.len() >= 2, "Expected at least 2 branches, got {}", results.len());

    // Verify branch names include 'dev'
    let branch_names: Vec<String> = results.iter()
        .map(|t| match &t.values[0] {
            heliosdb_nano::Value::String(s) => s.clone(),
            _ => String::new(),
        })
        .collect();

    assert!(branch_names.contains(&"dev".to_string()), "Branch 'dev' not found in {:?}", branch_names);
    assert!(branch_names.contains(&"main".to_string()), "Branch 'main' not found in {:?}", branch_names);
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_branch_drop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create a branch
    execute_ddl(&db, "CREATE DATABASE BRANCH temp FROM CURRENT AS OF NOW").unwrap();

    // Drop the branch
    execute_ddl(&db, "DROP DATABASE BRANCH temp").unwrap();

    // Verify branch is dropped by listing branches
    let results = execute_sql(&db, "SELECT * FROM pg_database_branches()").unwrap();
    let branch_names: Vec<String> = results.iter()
        .map(|t| match &t.values[0] {
            heliosdb_nano::Value::String(s) => s.clone(),
            _ => String::new(),
        })
        .collect();

    assert!(!branch_names.contains(&"temp".to_string()), "Branch 'temp' should have been dropped");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_branch_drop_if_exists() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Drop non-existent branch with IF EXISTS - should not error
    let result = execute_ddl(&db, "DROP DATABASE BRANCH IF EXISTS nonexistent");
    assert!(result.is_ok(), "DROP BRANCH IF EXISTS should not error for non-existent branch");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_branch_merge() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create source and target branches
    execute_ddl(&db, "CREATE DATABASE BRANCH staging FROM CURRENT AS OF NOW").unwrap();
    execute_ddl(&db, "CREATE DATABASE BRANCH production FROM CURRENT AS OF NOW").unwrap();

    // Merge staging into production
    execute_ddl(&db, "MERGE DATABASE BRANCH staging INTO production").unwrap();

    // Verify merge completed by listing branches and checking state
    let results = execute_sql(&db, "SELECT * FROM pg_database_branches()").unwrap();
    assert!(!results.is_empty(), "Should have branches after merge");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_materialized_view_create_and_refresh() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create base table
    execute_ddl(&db, "CREATE TABLE orders (id INT, status TEXT, amount INT)").unwrap();
    execute_ddl(&db, "INSERT INTO orders VALUES (1, 'pending', 100)").unwrap();
    execute_ddl(&db, "INSERT INTO orders VALUES (2, 'completed', 200)").unwrap();
    execute_ddl(&db, "INSERT INTO orders VALUES (3, 'pending', 150)").unwrap();

    // Create materialized view
    execute_ddl(&db, "CREATE MATERIALIZED VIEW order_summary AS SELECT status, COUNT(*) as count, SUM(amount) as total FROM orders GROUP BY status").unwrap();

    // Query materialized view
    let results = execute_sql(&db, "SELECT * FROM __mv_order_summary").unwrap();
    assert_eq!(results.len(), 2, "Expected 2 status groups (pending and completed)");

    // Insert more data
    execute_ddl(&db, "INSERT INTO orders VALUES (4, 'completed', 300)").unwrap();

    // Refresh materialized view
    execute_ddl(&db, "REFRESH MATERIALIZED VIEW order_summary").unwrap();

    // Query again - should reflect new data
    let results = execute_sql(&db, "SELECT * FROM __mv_order_summary").unwrap();
    assert_eq!(results.len(), 2, "Still 2 status groups after refresh");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_materialized_view_concurrent_refresh() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create base table
    execute_ddl(&db, "CREATE TABLE products (id INT, name TEXT, price INT)").unwrap();
    execute_ddl(&db, "INSERT INTO products VALUES (1, 'Widget', 10)").unwrap();
    execute_ddl(&db, "INSERT INTO products VALUES (2, 'Gadget', 20)").unwrap();

    // Create materialized view
    execute_ddl(&db, "CREATE MATERIALIZED VIEW product_stats AS SELECT COUNT(*) as total, AVG(price) as avg_price FROM products").unwrap();

    // Refresh concurrently (zero downtime)
    execute_ddl(&db, "REFRESH MATERIALIZED VIEW CONCURRENTLY product_stats").unwrap();

    // Verify data is still accessible
    let results = execute_sql(&db, "SELECT * FROM __mv_product_stats").unwrap();
    assert_eq!(results.len(), 1, "Product stats should have 1 row");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_materialized_view_drop() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create base table
    execute_ddl(&db, "CREATE TABLE users (id INT, name TEXT)").unwrap();

    // Create materialized view
    execute_ddl(&db, "CREATE MATERIALIZED VIEW user_count AS SELECT COUNT(*) FROM users").unwrap();

    // Drop materialized view
    execute_ddl(&db, "DROP MATERIALIZED VIEW user_count").unwrap();

    // Verify view is dropped by trying to query it (should fail or return error)
    let result = execute_sql(&db, "SELECT * FROM __mv_user_count");
    assert!(result.is_err(), "Query to dropped MV should fail");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_materialized_view_duplicate_error() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create base table
    execute_ddl(&db, "CREATE TABLE items (id INT)").unwrap();

    // Create materialized view
    execute_ddl(&db, "CREATE MATERIALIZED VIEW item_stats AS SELECT COUNT(*) FROM items").unwrap();

    // Try to create again - should error because it already exists
    let result = execute_ddl(&db, "CREATE MATERIALIZED VIEW item_stats AS SELECT COUNT(*) FROM items");
    assert!(result.is_err(), "Creating duplicate MV should error");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_time_travel_as_of_now() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create table and insert data
    execute_ddl(&db, "CREATE TABLE history (id INT, value TEXT)").unwrap();
    execute_ddl(&db, "INSERT INTO history VALUES (1, 'initial')").unwrap();

    // Query AS OF NOW (should return current data)
    let results = execute_sql(&db, "SELECT * FROM history AS OF NOW").unwrap();
    assert_eq!(results.len(), 1, "Should have 1 row");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_system_view_pg_database_branches() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create a few branches
    execute_ddl(&db, "CREATE DATABASE BRANCH dev FROM CURRENT AS OF NOW").unwrap();
    execute_ddl(&db, "CREATE DATABASE BRANCH staging FROM CURRENT AS OF NOW").unwrap();

    // Query system view
    let results = execute_sql(&db, "SELECT * FROM pg_database_branches()").unwrap();

    // Should have at least 3 branches (main, dev, staging)
    assert!(results.len() >= 3, "Expected at least 3 branches, got {}", results.len());

    // Each result should have columns (verify structure)
    for result in &results {
        assert!(!result.values.is_empty(), "Branch result should have values");
    }
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_system_view_pg_mv_staleness() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create base table and materialized view
    execute_ddl(&db, "CREATE TABLE data (id INT)").unwrap();
    execute_ddl(&db, "CREATE MATERIALIZED VIEW data_summary AS SELECT COUNT(*) FROM data").unwrap();

    // Query staleness view
    let results = execute_sql(&db, "SELECT * FROM pg_mv_staleness()").unwrap();

    // Should have 1 materialized view
    assert_eq!(results.len(), 1, "Should have 1 MV in staleness view");

    // Verify view name is correct
    match &results[0].values[0] {
        heliosdb_nano::Value::String(name) => {
            assert_eq!(name, "data_summary", "MV name should be 'data_summary'");
        }
        _ => panic!("Expected String value for view name"),
    }
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_branch_with_options() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create branch with simple options (key=value format without parentheses)
    execute_ddl(&db, "CREATE DATABASE BRANCH dev FROM CURRENT AS OF NOW WITH replication_factor=3").unwrap();

    // Verify branch exists by listing branches
    let results = execute_sql(&db, "SELECT * FROM pg_database_branches()").unwrap();
    let branch_names: Vec<String> = results.iter()
        .map(|t| match &t.values[0] {
            heliosdb_nano::Value::String(s) => s.clone(),
            _ => String::new(),
        })
        .collect();

    assert!(branch_names.contains(&"dev".to_string()), "Branch 'dev' should exist");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_merge_with_conflict_resolution() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create branches
    execute_ddl(&db, "CREATE DATABASE BRANCH branch1 FROM CURRENT AS OF NOW").unwrap();
    execute_ddl(&db, "CREATE DATABASE BRANCH branch2 FROM CURRENT AS OF NOW").unwrap();

    // Merge with conflict resolution strategy (using simple key=value format)
    execute_ddl(&db, "MERGE DATABASE BRANCH branch1 INTO branch2 WITH conflict_resolution=branch_wins").unwrap();

    // Verify merge completed by checking branches exist
    let results = execute_sql(&db, "SELECT * FROM pg_database_branches()").unwrap();
    assert!(!results.is_empty(), "Should have branches after merge");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_error_handling_invalid_branch() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Try to drop non-existent branch without IF EXISTS
    let result = execute_ddl(&db, "DROP DATABASE BRANCH nonexistent");
    assert!(result.is_err(), "DROP non-existent branch should error");

    // Try to merge non-existent branch
    let result = execute_ddl(&db, "MERGE DATABASE BRANCH nonexistent INTO main");
    assert!(result.is_err(), "MERGE non-existent branch should error");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_error_handling_invalid_mv() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Try to refresh non-existent MV
    let result = execute_ddl(&db, "REFRESH MATERIALIZED VIEW nonexistent");
    assert!(result.is_err(), "REFRESH non-existent MV should error");

    // Try to drop non-existent MV without IF EXISTS
    let result = execute_ddl(&db, "DROP MATERIALIZED VIEW nonexistent");
    assert!(result.is_err(), "DROP non-existent MV should error");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_end_to_end_branch_workflow() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create development branch
    execute_ddl(&db, "CREATE DATABASE BRANCH development FROM CURRENT AS OF NOW").unwrap();

    // Create feature branch from development
    execute_ddl(&db, "CREATE DATABASE BRANCH feature FROM development AS OF NOW").unwrap();

    // List branches
    let results = execute_sql(&db, "SELECT * FROM pg_database_branches()").unwrap();
    assert!(results.len() >= 3, "Should have at least 3 branches (main, development, feature)");

    // Merge feature into development
    execute_ddl(&db, "MERGE DATABASE BRANCH feature INTO development").unwrap();

    // Merge development into main
    execute_ddl(&db, "MERGE DATABASE BRANCH development INTO main").unwrap();

    // Verify all branches still queryable
    let final_results = execute_sql(&db, "SELECT * FROM pg_database_branches()").unwrap();
    assert!(!final_results.is_empty(), "Should have branches after merges");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_end_to_end_materialized_view_workflow() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create base tables
    execute_ddl(&db, "CREATE TABLE customers (id INT, name TEXT, tier TEXT)").unwrap();
    execute_ddl(&db, "CREATE TABLE orders (customer_id INT, amount INT, status TEXT)").unwrap();

    // Insert sample data
    execute_ddl(&db, "INSERT INTO customers VALUES (1, 'Alice', 'gold')").unwrap();
    execute_ddl(&db, "INSERT INTO customers VALUES (2, 'Bob', 'silver')").unwrap();
    execute_ddl(&db, "INSERT INTO orders VALUES (1, 100, 'completed')").unwrap();
    execute_ddl(&db, "INSERT INTO orders VALUES (1, 200, 'completed')").unwrap();
    execute_ddl(&db, "INSERT INTO orders VALUES (2, 50, 'pending')").unwrap();

    // Create materialized view with aggregation
    execute_ddl(&db, "CREATE MATERIALIZED VIEW customer_stats AS SELECT c.tier, COUNT(*) as customer_count FROM customers c GROUP BY c.tier").unwrap();

    // Query materialized view
    let results = execute_sql(&db, "SELECT * FROM __mv_customer_stats").unwrap();
    assert_eq!(results.len(), 2, "Should have 2 tiers (gold and silver)");

    // Check staleness via system view
    let staleness = execute_sql(&db, "SELECT * FROM pg_mv_staleness()").unwrap();
    assert_eq!(staleness.len(), 1, "Should have 1 MV in staleness view");

    // Insert more customers
    execute_ddl(&db, "INSERT INTO customers VALUES (3, 'Charlie', 'bronze')").unwrap();

    // Refresh concurrently
    execute_ddl(&db, "REFRESH MATERIALIZED VIEW CONCURRENTLY customer_stats").unwrap();

    // Verify updated data
    let results = execute_sql(&db, "SELECT * FROM __mv_customer_stats").unwrap();
    assert_eq!(results.len(), 3, "Should have 3 tiers after refresh (gold, silver, bronze)");

    // Clean up
    execute_ddl(&db, "DROP MATERIALIZED VIEW customer_stats").unwrap();

    // Verify cleanup
    let result = execute_sql(&db, "SELECT * FROM __mv_customer_stats");
    assert!(result.is_err(), "Query to dropped MV should fail");
}

#[test]
#[allow(clippy::unwrap_used)]
fn test_materialized_view_with_options() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create base table
    execute_ddl(&db, "CREATE TABLE metrics (value INT)").unwrap();
    execute_ddl(&db, "INSERT INTO metrics VALUES (42)").unwrap();

    // Create MV with options
    execute_ddl(&db, "CREATE MATERIALIZED VIEW metric_summary AS SELECT AVG(value) as avg_value FROM metrics WITH (auto_refresh=true, max_cpu_percent=15)").unwrap();

    // Verify MV exists and is queryable
    let results = execute_sql(&db, "SELECT * FROM __mv_metric_summary").unwrap();
    assert_eq!(results.len(), 1, "MV should have 1 row");
}
