//! End-to-end integration test for Incremental Materialized Views
//!
//! This test verifies the complete integration of incremental MV refresh:
//! 1. CREATE TABLE (base table)
//! 2. INSERT initial data
//! 3. CREATE MATERIALIZED VIEW (with delta tracking)
//! 4. INSERT new data to base table (delta tracking captures changes)
//! 5. REFRESH MATERIALIZED VIEW (applies incremental changes)
//! 6. Verify results

use heliosdb_nano::{Config, EmbeddedDatabase, Result};

#[test]
fn test_incremental_mv_full_integration() -> Result<()> {
    // Create in-memory database
    let _config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory()?;

    // Step 1: Create base table
    db.execute("CREATE TABLE orders (order_id INT, customer_id INT, amount FLOAT, status TEXT)")?;

    // Step 2: Insert initial data
    db.execute("INSERT INTO orders VALUES (1, 100, 50.0, 'completed')")?;
    db.execute("INSERT INTO orders VALUES (2, 101, 75.5, 'completed')")?;
    db.execute("INSERT INTO orders VALUES (3, 100, 25.0, 'pending')")?;

    // Step 3: Create materialized view
    db.execute("CREATE MATERIALIZED VIEW completed_orders AS SELECT * FROM orders WHERE status = 'completed'")?;

    // Verify initial MV data
    let result = db.query("SELECT * FROM completed_orders", &[])?;
    assert_eq!(result.len(), 2, "Should have 2 completed orders initially");

    // Step 4: Insert new data (this should be tracked by delta tracker)
    db.execute("INSERT INTO orders VALUES (4, 102, 100.0, 'completed')")?;
    db.execute("INSERT INTO orders VALUES (5, 103, 30.0, 'pending')")?;

    // Step 5: Refresh materialized view (should use incremental refresh if possible)
    db.execute("REFRESH MATERIALIZED VIEW completed_orders")?;

    // Step 6: Verify refreshed data
    let result = db.query("SELECT * FROM completed_orders", &[])?;
    assert_eq!(result.len(), 3, "Should have 3 completed orders after refresh");

    // Verify the new completed order is present
    let order_4_present = result.iter().any(|tuple| {
        tuple.values.len() >= 1 && tuple.values[0] == heliosdb_nano::Value::Int4(4)
    });
    assert!(order_4_present, "New completed order (ID 4) should be in materialized view");

    println!("✓ Incremental MV integration test passed!");
    Ok(())
}

#[test]
fn test_incremental_mv_aggregate() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table
    db.execute("CREATE TABLE sales (product_id INT, quantity INT, price FLOAT)")?;

    // Insert initial data
    db.execute("INSERT INTO sales VALUES (1, 10, 5.0)")?;
    db.execute("INSERT INTO sales VALUES (2, 5, 10.0)")?;
    db.execute("INSERT INTO sales VALUES (1, 15, 5.0)")?;

    // Create aggregate materialized view
    db.execute("CREATE MATERIALIZED VIEW sales_summary AS SELECT product_id, SUM(quantity) as total_qty FROM sales GROUP BY product_id")?;

    // Verify initial aggregates
    let result = db.query("SELECT * FROM sales_summary ORDER BY product_id", &[])?;
    assert_eq!(result.len(), 2, "Should have 2 products initially");

    // Insert more data
    db.execute("INSERT INTO sales VALUES (1, 5, 5.0)")?;
    db.execute("INSERT INTO sales VALUES (3, 20, 8.0)")?;

    // Refresh (should apply incremental updates to aggregates)
    db.execute("REFRESH MATERIALIZED VIEW sales_summary")?;

    // Verify updated aggregates
    let result = db.query("SELECT * FROM sales_summary ORDER BY product_id", &[])?;
    assert_eq!(result.len(), 3, "Should have 3 products after refresh");

    println!("✓ Incremental MV aggregate test passed!");
    Ok(())
}

#[test]
fn test_delta_tracker_basic() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table
    db.execute("CREATE TABLE events (event_id INT, event_type TEXT, timestamp INT)")?;

    // Insert initial data
    db.execute("INSERT INTO events VALUES (1, 'login', 1000)")?;
    db.execute("INSERT INTO events VALUES (2, 'logout', 2000)")?;

    // Create MV to test delta tracking implicitly
    db.execute("CREATE MATERIALIZED VIEW login_events AS SELECT * FROM events WHERE event_type = 'login'")?;

    // Verify initial MV state
    let result = db.query("SELECT * FROM login_events", &[])?;
    assert_eq!(result.len(), 1, "Should have 1 login event initially");

    // Insert more data (delta tracker should capture this)
    db.execute("INSERT INTO events VALUES (3, 'login', 3000)")?;
    db.execute("INSERT INTO events VALUES (4, 'logout', 4000)")?;

    // Refresh MV (should use delta tracking if available)
    db.execute("REFRESH MATERIALIZED VIEW login_events")?;

    // Verify MV was updated correctly (implicitly tests delta tracker)
    let result = db.query("SELECT * FROM login_events", &[])?;
    assert_eq!(result.len(), 2, "Should have 2 login events after refresh");

    println!("✓ Delta tracker basic test passed!");
    Ok(())
}

#[test]
fn test_mv_schema_mapping() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table with multiple columns
    db.execute("CREATE TABLE products (product_id INT, name TEXT, price FLOAT, category TEXT)")?;

    // Insert data
    db.execute("INSERT INTO products VALUES (1, 'Widget', 9.99, 'Tools')")?;
    db.execute("INSERT INTO products VALUES (2, 'Gadget', 19.99, 'Electronics')")?;
    db.execute("INSERT INTO products VALUES (3, 'Doohickey', 14.99, 'Tools')")?;

    // Create MV with column name references in filter
    db.execute("CREATE MATERIALIZED VIEW tool_products AS SELECT product_id, name, price FROM products WHERE category = 'Tools'")?;

    // Verify initial data
    let result = db.query("SELECT * FROM tool_products", &[])?;
    assert_eq!(result.len(), 2, "Should have 2 tool products");

    // Insert new tool product
    db.execute("INSERT INTO products VALUES (4, 'Thingamajig', 7.99, 'Tools')")?;

    // Refresh (should correctly map column names to indices)
    db.execute("REFRESH MATERIALIZED VIEW tool_products")?;

    // Verify schema mapping worked correctly
    let result = db.query("SELECT * FROM tool_products", &[])?;
    assert_eq!(result.len(), 3, "Should have 3 tool products after refresh with schema mapping");

    println!("✓ MV schema mapping test passed!");
    Ok(())
}

#[test]
fn test_concurrent_vs_regular_refresh() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table
    db.execute("CREATE TABLE items (item_id INT, status TEXT)")?;

    // Insert initial data
    db.execute("INSERT INTO items VALUES (1, 'active')")?;
    db.execute("INSERT INTO items VALUES (2, 'active')")?;

    // Create MV
    db.execute("CREATE MATERIALIZED VIEW active_items AS SELECT * FROM items WHERE status = 'active'")?;

    // Test regular refresh
    db.execute("INSERT INTO items VALUES (3, 'active')")?;
    db.execute("REFRESH MATERIALIZED VIEW active_items")?;
    let result = db.query("SELECT * FROM active_items", &[])?;
    assert_eq!(result.len(), 3, "Regular refresh should work");

    // Test concurrent refresh (should fall back to full refresh for now)
    db.execute("INSERT INTO items VALUES (4, 'active')")?;
    db.execute("REFRESH MATERIALIZED VIEW CONCURRENTLY active_items")?;
    let result = db.query("SELECT * FROM active_items", &[])?;
    assert_eq!(result.len(), 4, "Concurrent refresh should work (via full refresh)");

    println!("✓ Concurrent vs regular refresh test passed!");
    Ok(())
}

#[test]
fn test_refresh_mv_incrementally_keyword() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table
    db.execute("CREATE TABLE users (user_id INT, username TEXT, active INT)")?;

    // Insert initial data
    db.execute("INSERT INTO users VALUES (1, 'alice', 1)")?;
    db.execute("INSERT INTO users VALUES (2, 'bob', 1)")?;
    db.execute("INSERT INTO users VALUES (3, 'charlie', 0)")?;

    // Create MV
    db.execute("CREATE MATERIALIZED VIEW active_users AS SELECT * FROM users WHERE active = 1")?;

    // Verify initial data
    let result = db.query("SELECT * FROM active_users", &[])?;
    assert_eq!(result.len(), 2, "Should have 2 active users initially");

    // Insert new user
    db.execute("INSERT INTO users VALUES (4, 'diana', 1)")?;

    // Refresh with explicit INCREMENTALLY keyword
    // This should trigger incremental refresh path (even if it falls back to full refresh)
    db.execute("REFRESH MATERIALIZED VIEW active_users INCREMENTALLY")?;

    // Verify refreshed data
    let result = db.query("SELECT * FROM active_users", &[])?;
    assert_eq!(result.len(), 3, "Should have 3 active users after incremental refresh");

    println!("✓ REFRESH MATERIALIZED VIEW INCREMENTALLY test passed!");
    Ok(())
}

#[test]
fn test_refresh_mv_incrementally_no_prior_refresh() -> Result<()> {
    // Test that INCREMENTALLY gracefully falls back when MV was never refreshed
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table and MV
    db.execute("CREATE TABLE logs (log_id INT, level TEXT)")?;
    db.execute("INSERT INTO logs VALUES (1, 'error')")?;
    db.execute("CREATE MATERIALIZED VIEW error_logs AS SELECT * FROM logs WHERE level = 'error'")?;

    // Initial data is available (CREATE populates MV)
    let result = db.query("SELECT * FROM error_logs", &[])?;
    assert_eq!(result.len(), 1, "Should have 1 error log initially");

    // Add more data
    db.execute("INSERT INTO logs VALUES (2, 'error')")?;

    // INCREMENTALLY should still work (will use incremental if possible, else full)
    db.execute("REFRESH MATERIALIZED VIEW error_logs INCREMENTALLY")?;

    let result = db.query("SELECT * FROM error_logs", &[])?;
    assert_eq!(result.len(), 2, "Should have 2 error logs after refresh");

    println!("✓ REFRESH INCREMENTALLY with prior refresh test passed!");
    Ok(())
}

#[test]
fn test_alter_materialized_view_set() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table and MV
    db.execute("CREATE TABLE metrics (metric_id INT, value FLOAT)")?;
    db.execute("INSERT INTO metrics VALUES (1, 100.0)")?;
    db.execute("CREATE MATERIALIZED VIEW metric_summary AS SELECT * FROM metrics")?;

    // Alter MV options - set staleness threshold (numeric seconds)
    db.execute("ALTER MATERIALIZED VIEW metric_summary SET (staleness_threshold = 1800)")?;

    // Alter MV options - set max CPU percent
    db.execute("ALTER MATERIALIZED VIEW metric_summary SET (max_cpu_percent = 25)")?;

    // Alter MV options - multiple options at once (staleness in seconds, priority as numeric)
    db.execute("ALTER MATERIALIZED VIEW metric_summary SET (staleness_threshold = 3600, max_cpu_percent = 15, incremental_enabled = true)")?;

    // MV should still work correctly after alterations
    db.execute("INSERT INTO metrics VALUES (2, 200.0)")?;
    db.execute("REFRESH MATERIALIZED VIEW metric_summary")?;

    let result = db.query("SELECT * FROM metric_summary", &[])?;
    assert_eq!(result.len(), 2, "MV should have 2 rows after refresh");

    println!("✓ ALTER MATERIALIZED VIEW SET test passed!");
    Ok(())
}

#[test]
fn test_alter_mv_invalid_option_value() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table and MV
    db.execute("CREATE TABLE data (id INT)")?;
    db.execute("CREATE MATERIALIZED VIEW data_view AS SELECT * FROM data")?;

    // Try to set a non-numeric value for numeric option - should fail
    let result = db.execute("ALTER MATERIALIZED VIEW data_view SET (max_cpu_percent = 'not_a_number')");
    assert!(result.is_err(), "Should error on non-numeric value for numeric option");

    let error_msg = result.unwrap_err().to_string();
    assert!(
        error_msg.contains("numeric") || error_msg.contains("requires"),
        "Error message should mention numeric requirement: {}", error_msg
    );

    // Try to set invalid refresh_strategy - should fail
    let result = db.execute("ALTER MATERIALIZED VIEW data_view SET (refresh_strategy = 'invalid_strategy')");
    assert!(result.is_err(), "Should error on invalid refresh_strategy");

    println!("✓ ALTER MV invalid option value test passed!");
    Ok(())
}

#[test]
fn test_alter_mv_refresh_strategy() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table and MV
    db.execute("CREATE TABLE events (event_id INT, event_type TEXT)")?;
    db.execute("INSERT INTO events VALUES (1, 'click')")?;
    db.execute("CREATE MATERIALIZED VIEW click_events AS SELECT * FROM events WHERE event_type = 'click'")?;

    // Set refresh strategy to incremental
    db.execute("ALTER MATERIALIZED VIEW click_events SET (refresh_strategy = 'incremental')")?;

    // Add data and refresh
    db.execute("INSERT INTO events VALUES (2, 'click')")?;
    db.execute("REFRESH MATERIALIZED VIEW click_events")?;

    let result = db.query("SELECT * FROM click_events", &[])?;
    assert_eq!(result.len(), 2, "Should have 2 click events");

    // Set refresh strategy to manual (disables auto-refresh)
    db.execute("ALTER MATERIALIZED VIEW click_events SET (refresh_strategy = 'manual')")?;

    // Add data and refresh
    db.execute("INSERT INTO events VALUES (3, 'click')")?;
    db.execute("REFRESH MATERIALIZED VIEW click_events")?;

    let result = db.query("SELECT * FROM click_events", &[])?;
    assert_eq!(result.len(), 3, "Should have 3 click events");

    // Set refresh strategy to auto
    db.execute("ALTER MATERIALIZED VIEW click_events SET (refresh_strategy = 'auto')")?;

    println!("✓ ALTER MV refresh_strategy test passed!");
    Ok(())
}

#[test]
fn test_alter_mv_priority() -> Result<()> {
    // Create in-memory database
    let config = Config::in_memory();
    let db = EmbeddedDatabase::with_config(config)?;

    // Create base table and MV
    db.execute("CREATE TABLE orders (order_id INT, amount FLOAT)")?;
    db.execute("INSERT INTO orders VALUES (1, 100.0)")?;
    db.execute("CREATE MATERIALIZED VIEW order_summary AS SELECT * FROM orders")?;

    // Set priority levels as numeric (0=low, 1=medium, 2=high, 3=critical)
    db.execute("ALTER MATERIALIZED VIEW order_summary SET (priority = 2)")?;

    // Verify MV still works
    db.execute("INSERT INTO orders VALUES (2, 200.0)")?;
    db.execute("REFRESH MATERIALIZED VIEW order_summary")?;

    let result = db.query("SELECT * FROM order_summary", &[])?;
    assert_eq!(result.len(), 2, "Should have 2 orders");

    // Set to critical priority
    db.execute("ALTER MATERIALIZED VIEW order_summary SET (priority = 3)")?;

    println!("✓ ALTER MV priority test passed!");
    Ok(())
}
