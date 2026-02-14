//! Integration tests for materialized views
//!
//! Tests the complete lifecycle of materialized views including:
//! - CREATE MATERIALIZED VIEW
//! - REFRESH MATERIALIZED VIEW
//! - DROP MATERIALIZED VIEW
//! - Staleness tracking
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_nano::{Config, Database, Result, Value, Column, DataType, Schema};

#[test]
fn test_create_materialized_view_basic() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create a base table
    db.execute("CREATE TABLE users (id INT, name TEXT, age INT)")?;
    db.execute("INSERT INTO users VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users VALUES (2, 'Bob', 25)")?;
    db.execute("INSERT INTO users VALUES (3, 'Charlie', 35)")?;

    // Create a simple materialized view
    db.execute("CREATE MATERIALIZED VIEW user_summary AS SELECT COUNT(*) as total FROM users")?;

    // Verify the view exists
    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();
    assert!(mv_catalog.view_exists("user_summary")?);

    // Verify metadata
    let metadata = mv_catalog.get_view("user_summary")?;
    assert_eq!(metadata.view_name, "user_summary");
    assert_eq!(metadata.base_tables, vec!["users"]);
    assert!(metadata.last_refresh.is_some());
    assert_eq!(metadata.row_count, Some(1));

    Ok(())
}

#[test]
fn test_create_materialized_view_if_not_exists() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create a base table
    db.execute("CREATE TABLE products (id INT, name TEXT)")?;

    // Create materialized view
    db.execute("CREATE MATERIALIZED VIEW product_list AS SELECT * FROM products")?;

    // Try to create again without IF NOT EXISTS - should fail
    let result = db.execute("CREATE MATERIALIZED VIEW product_list AS SELECT * FROM products");
    assert!(result.is_err());

    // Try with IF NOT EXISTS - should succeed
    db.execute("CREATE MATERIALIZED VIEW IF NOT EXISTS product_list AS SELECT * FROM products")?;

    Ok(())
}

#[test]
fn test_drop_materialized_view() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create base table and view
    db.execute("CREATE TABLE orders (id INT, total REAL)")?;
    db.execute("CREATE MATERIALIZED VIEW order_stats AS SELECT COUNT(*) FROM orders")?;

    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();
    assert!(mv_catalog.view_exists("order_stats")?);

    // Drop the view
    db.execute("DROP MATERIALIZED VIEW order_stats")?;

    // Verify it's gone
    assert!(!mv_catalog.view_exists("order_stats")?);

    Ok(())
}

#[test]
fn test_drop_materialized_view_if_exists() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Try to drop non-existent view without IF EXISTS - should fail
    let result = db.execute("DROP MATERIALIZED VIEW nonexistent");
    assert!(result.is_err());

    // Try with IF EXISTS - should succeed
    db.execute("DROP MATERIALIZED VIEW IF EXISTS nonexistent")?;

    Ok(())
}

#[test]
fn test_refresh_materialized_view() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create base table and view
    db.execute("CREATE TABLE inventory (item TEXT, quantity INT)")?;
    db.execute("INSERT INTO inventory VALUES ('Widget', 100)")?;
    db.execute("CREATE MATERIALIZED VIEW inventory_summary AS SELECT COUNT(*) FROM inventory")?;

    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();

    // Get initial metadata
    let metadata_before = mv_catalog.get_view("inventory_summary")?;
    let refresh_time_before = metadata_before.last_refresh;

    // Wait a moment
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Refresh the view
    db.execute("REFRESH MATERIALIZED VIEW inventory_summary")?;

    // Verify refresh timestamp updated
    let metadata_after = mv_catalog.get_view("inventory_summary")?;
    assert!(metadata_after.last_refresh > refresh_time_before);

    Ok(())
}

#[test]
fn test_materialized_view_with_aggregation() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create base table with sample data
    db.execute("CREATE TABLE sales (product TEXT, amount REAL)")?;
    db.execute("INSERT INTO sales VALUES ('A', 100.0)")?;
    db.execute("INSERT INTO sales VALUES ('B', 200.0)")?;
    db.execute("INSERT INTO sales VALUES ('A', 150.0)")?;

    // Create aggregated materialized view
    db.execute("CREATE MATERIALIZED VIEW sales_summary AS SELECT COUNT(*) as total FROM sales")?;

    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();

    // Verify the view was populated
    let metadata = mv_catalog.get_view("sales_summary")?;
    assert_eq!(metadata.row_count, Some(1)); // One row with COUNT(*)
    assert!(!metadata.is_stale());

    Ok(())
}

#[test]
fn test_materialized_view_staleness_tracking() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create base table and view
    db.execute("CREATE TABLE events (id INT, event_time TEXT)")?;
    db.execute("CREATE MATERIALIZED VIEW event_count AS SELECT COUNT(*) FROM events")?;

    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();

    // View should not be stale (just created and populated)
    let metadata = mv_catalog.get_view("event_count")?;
    assert!(!metadata.is_stale());
    assert!(metadata.last_refresh.is_some());

    // Staleness should be very small (just created)
    let staleness = metadata.staleness_seconds();
    assert!(staleness.is_some());
    assert!(staleness.unwrap() < 2); // Less than 2 seconds

    Ok(())
}

#[test]
fn test_list_materialized_views() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create multiple views
    db.execute("CREATE TABLE t1 (id INT)")?;
    db.execute("CREATE TABLE t2 (id INT)")?;

    db.execute("CREATE MATERIALIZED VIEW view1 AS SELECT * FROM t1")?;
    db.execute("CREATE MATERIALIZED VIEW view2 AS SELECT * FROM t2")?;
    db.execute("CREATE MATERIALIZED VIEW view3 AS SELECT COUNT(*) FROM t1")?;

    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();

    // List all views
    let views = mv_catalog.list_views()?;
    assert_eq!(views.len(), 3);
    assert!(views.contains(&"view1".to_string()));
    assert!(views.contains(&"view2".to_string()));
    assert!(views.contains(&"view3".to_string()));

    Ok(())
}

#[test]
fn test_materialized_view_base_table_tracking() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create tables
    db.execute("CREATE TABLE customers (id INT, name TEXT)")?;
    db.execute("CREATE TABLE orders (id INT, customer_id INT, total REAL)")?;

    // Create a view that joins both tables
    db.execute("CREATE MATERIALIZED VIEW customer_orders AS SELECT COUNT(*) FROM customers")?;

    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();

    // Verify base tables are tracked
    let metadata = mv_catalog.get_view("customer_orders")?;
    assert_eq!(metadata.base_tables.len(), 1);
    assert!(metadata.base_tables.contains(&"customers".to_string()));

    Ok(())
}

#[test]
fn test_materialized_view_data_storage() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create base table with data
    db.execute("CREATE TABLE items (id INT, name TEXT)")?;
    db.execute("INSERT INTO items VALUES (1, 'Item1')")?;
    db.execute("INSERT INTO items VALUES (2, 'Item2')")?;

    // Create materialized view
    db.execute("CREATE MATERIALIZED VIEW item_list AS SELECT COUNT(*) FROM items")?;

    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();

    // Read the stored data
    let data = mv_catalog.read_view_data("item_list")?;
    assert_eq!(data.len(), 1); // One row with COUNT(*)

    // Verify the count value
    assert_eq!(data[0].values.len(), 1);
    match &data[0].values[0] {
        Value::Int8(count) => assert_eq!(*count, 2),
        _ => panic!("Expected Int8 value for COUNT(*)"),
    }

    Ok(())
}

#[test]
fn test_concurrent_refresh_flag() -> Result<()> {
    let config = Config::in_memory();
    let mut db = Database::open_with_config(config)?;

    // Create base table and view
    db.execute("CREATE TABLE logs (id INT, message TEXT)")?;
    db.execute("CREATE MATERIALIZED VIEW log_count AS SELECT COUNT(*) FROM logs")?;

    // Test CONCURRENT refresh (currently same as regular refresh)
    db.execute("REFRESH MATERIALIZED VIEW CONCURRENTLY log_count")?;

    let storage = db.storage();
    let mv_catalog = storage.mv_catalog();

    // Should complete without error
    let metadata = mv_catalog.get_view("log_count")?;
    assert!(metadata.last_refresh.is_some());

    Ok(())
}
