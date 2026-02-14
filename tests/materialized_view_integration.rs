//! Materialized View Integration Tests
//!
//! Tests for CREATE/REFRESH/DROP MATERIALIZED VIEW using the public API.

use heliosdb_nano::{EmbeddedDatabase, Result};

#[test]
fn test_create_materialized_view() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create base table
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, status TEXT, amount INT)")?;

    // Insert data
    db.execute("INSERT INTO orders VALUES (1, 'completed', 100)")?;
    db.execute("INSERT INTO orders VALUES (2, 'completed', 200)")?;
    db.execute("INSERT INTO orders VALUES (3, 'pending', 50)")?;

    // Create materialized view
    let result = db.execute(
        "CREATE MATERIALIZED VIEW completed_orders AS SELECT id, amount FROM orders WHERE status = 'completed'"
    );
    assert!(result.is_ok(), "CREATE MATERIALIZED VIEW should succeed: {:?}", result.err());

    Ok(())
}

#[test]
fn test_query_materialized_view() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create base table
    db.execute("CREATE TABLE products (id INT PRIMARY KEY, name TEXT, price INT)")?;

    // Insert data
    db.execute("INSERT INTO products VALUES (1, 'Laptop', 1000)")?;
    db.execute("INSERT INTO products VALUES (2, 'Mouse', 25)")?;
    db.execute("INSERT INTO products VALUES (3, 'Keyboard', 75)")?;

    // Create materialized view
    db.execute(
        "CREATE MATERIALIZED VIEW expensive_products AS SELECT id, name, price FROM products WHERE price > 50"
    )?;

    // Query the materialized view
    let empty_params: &[&dyn std::fmt::Display] = &[];
    let results = db.query("SELECT * FROM expensive_products", empty_params)?;

    // Should have 2 rows (Laptop and Keyboard)
    assert_eq!(results.len(), 2, "Expected 2 rows in materialized view, got {}", results.len());

    Ok(())
}

#[test]
fn test_refresh_materialized_view() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create base table
    db.execute("CREATE TABLE sales (id INT PRIMARY KEY, amount INT)")?;

    // Insert initial data
    db.execute("INSERT INTO sales VALUES (1, 100)")?;

    // Create materialized view with aggregation
    db.execute(
        "CREATE MATERIALIZED VIEW sales_summary AS SELECT SUM(amount) as total FROM sales"
    )?;

    // Check initial value
    let empty_params: &[&dyn std::fmt::Display] = &[];
    let results = db.query("SELECT * FROM sales_summary", empty_params)?;
    assert_eq!(results.len(), 1);

    // Insert more data
    db.execute("INSERT INTO sales VALUES (2, 200)")?;

    // Refresh the view
    let refresh_result = db.execute("REFRESH MATERIALIZED VIEW sales_summary");
    assert!(refresh_result.is_ok(), "REFRESH should succeed: {:?}", refresh_result.err());

    // Query again - should now reflect updated data
    let results = db.query("SELECT * FROM sales_summary", empty_params)?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_create_mv_duplicate_fails() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create base table
    db.execute("CREATE TABLE items (id INT PRIMARY KEY)")?;
    db.execute("INSERT INTO items VALUES (1)")?;

    // Create MV first time
    db.execute("CREATE MATERIALIZED VIEW item_list AS SELECT * FROM items")?;

    // Create again without IF NOT EXISTS should fail
    let result = db.execute("CREATE MATERIALIZED VIEW item_list AS SELECT * FROM items");
    assert!(result.is_err(), "Should fail when MV already exists");

    // Note: IF NOT EXISTS syntax is not currently supported by sqlparser for materialized views
    // This would be added via custom parsing if needed

    Ok(())
}

#[test]
fn test_refresh_after_insert() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let empty_params: &[&dyn std::fmt::Display] = &[];

    // Create base table
    db.execute("CREATE TABLE inventory (id INT PRIMARY KEY, item TEXT, quantity INT)")?;

    // Insert initial data
    db.execute("INSERT INTO inventory VALUES (1, 'Widget', 10)")?;
    db.execute("INSERT INTO inventory VALUES (2, 'Gadget', 5)")?;

    // Create materialized view
    db.execute(
        "CREATE MATERIALIZED VIEW inventory_summary AS SELECT id, item, quantity FROM inventory WHERE quantity > 3"
    )?;

    // Query - should have 2 rows initially
    let results = db.query("SELECT * FROM inventory_summary", empty_params)?;
    assert_eq!(results.len(), 2, "Expected 2 rows after initial creation");

    // Insert more data after MV creation (this should be tracked for incremental refresh)
    db.execute("INSERT INTO inventory VALUES (3, 'Doodad', 20)")?;
    db.execute("INSERT INTO inventory VALUES (4, 'Thingamajig', 1)")?; // Won't match filter

    // Refresh - delta tracking should record the inserts
    db.execute("REFRESH MATERIALIZED VIEW inventory_summary")?;

    // Query again - should now have 3 rows (Doodad matches filter)
    let results = db.query("SELECT * FROM inventory_summary", empty_params)?;
    assert_eq!(results.len(), 3, "Expected 3 rows after refresh with new inserts");

    Ok(())
}

#[test]
fn test_refresh_after_update() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let empty_params: &[&dyn std::fmt::Display] = &[];

    // Create base table
    db.execute("CREATE TABLE stock (id INT PRIMARY KEY, product TEXT, price INT)")?;

    // Insert initial data
    db.execute("INSERT INTO stock VALUES (1, 'Apple', 100)")?;
    db.execute("INSERT INTO stock VALUES (2, 'Banana', 50)")?;
    db.execute("INSERT INTO stock VALUES (3, 'Cherry', 200)")?;

    // Create materialized view for expensive items
    db.execute(
        "CREATE MATERIALIZED VIEW expensive_stock AS SELECT id, product, price FROM stock WHERE price >= 100"
    )?;

    // Query - should have 2 rows (Apple and Cherry)
    let results = db.query("SELECT * FROM expensive_stock", empty_params)?;
    assert_eq!(results.len(), 2, "Expected 2 rows initially (Apple, Cherry)");

    // Update Banana price to make it expensive
    db.execute("UPDATE stock SET price = 150 WHERE id = 2")?;

    // Refresh - delta tracking should record the update
    db.execute("REFRESH MATERIALIZED VIEW expensive_stock")?;

    // Query again - should now have 3 rows (Apple, Banana, Cherry)
    let results = db.query("SELECT * FROM expensive_stock", empty_params)?;
    assert_eq!(results.len(), 3, "Expected 3 rows after refresh (Banana now expensive)");

    Ok(())
}

#[test]
fn test_refresh_after_delete() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let empty_params: &[&dyn std::fmt::Display] = &[];

    // Create base table
    db.execute("CREATE TABLE customers (id INT PRIMARY KEY, name TEXT, active INT)")?;

    // Insert initial data
    db.execute("INSERT INTO customers VALUES (1, 'Alice', 1)")?;
    db.execute("INSERT INTO customers VALUES (2, 'Bob', 1)")?;
    db.execute("INSERT INTO customers VALUES (3, 'Charlie', 1)")?;

    // Create materialized view
    db.execute(
        "CREATE MATERIALIZED VIEW active_customers AS SELECT id, name FROM customers WHERE active = 1"
    )?;

    // Query - should have 3 rows
    let results = db.query("SELECT * FROM active_customers", empty_params)?;
    assert_eq!(results.len(), 3, "Expected 3 active customers initially");

    // Delete Bob
    db.execute("DELETE FROM customers WHERE id = 2")?;

    // Refresh - delta tracking should record the delete
    db.execute("REFRESH MATERIALIZED VIEW active_customers")?;

    // Query again - should now have 2 rows
    let results = db.query("SELECT * FROM active_customers", empty_params)?;
    assert_eq!(results.len(), 2, "Expected 2 active customers after delete");

    Ok(())
}

#[test]
fn test_drop_materialized_view() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let empty_params: &[&dyn std::fmt::Display] = &[];

    // Create base table
    db.execute("CREATE TABLE data (id INT PRIMARY KEY, value INT)")?;
    db.execute("INSERT INTO data VALUES (1, 100)")?;

    // Create MV
    db.execute("CREATE MATERIALIZED VIEW data_view AS SELECT * FROM data")?;

    // Query should work
    let results = db.query("SELECT * FROM data_view", empty_params)?;
    assert_eq!(results.len(), 1);

    // Drop MV
    db.execute("DROP MATERIALIZED VIEW data_view")?;

    // Query should now fail
    let result = db.query("SELECT * FROM data_view", empty_params);
    assert!(result.is_err(), "Query on dropped MV should fail");

    Ok(())
}

#[test]
fn test_drop_mv_if_exists() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Drop non-existent MV with IF EXISTS should succeed
    let result = db.execute("DROP MATERIALIZED VIEW IF EXISTS nonexistent_view");
    assert!(result.is_ok(), "DROP IF EXISTS on non-existent MV should succeed");

    // Drop non-existent MV without IF EXISTS should fail
    let result = db.execute("DROP MATERIALIZED VIEW nonexistent_view");
    assert!(result.is_err(), "DROP without IF EXISTS on non-existent MV should fail");

    Ok(())
}
