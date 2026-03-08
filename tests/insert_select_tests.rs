//! Tests for INSERT ... SELECT support
//!
//! Validates that INSERT INTO ... SELECT ... works correctly, including:
//! - Basic table-to-table copies
//! - SELECT with WHERE clause filtering
//! - Column subset targeting
//! - Expressions and functions in SELECT
//! - Aggregate queries as source
//! - Column count mismatch error handling
//! - Self-referential inserts (INSERT INTO t SELECT FROM t)

mod test_helpers;

use heliosdb_nano::{EmbeddedDatabase, Result, Value};
use test_helpers::create_test_db;

/// Helper: create a source table with test data
fn setup_source_table(db: &EmbeddedDatabase) -> Result<()> {
    db.execute(
        "CREATE TABLE source (id INT PRIMARY KEY, name TEXT, dept TEXT, salary INT)"
    )?;
    db.execute("INSERT INTO source (id, name, dept, salary) VALUES (1, 'Alice', 'Engineering', 90000)")?;
    db.execute("INSERT INTO source (id, name, dept, salary) VALUES (2, 'Bob', 'Engineering', 85000)")?;
    db.execute("INSERT INTO source (id, name, dept, salary) VALUES (3, 'Carol', 'Marketing', 75000)")?;
    db.execute("INSERT INTO source (id, name, dept, salary) VALUES (4, 'Dave', 'Marketing', 70000)")?;
    db.execute("INSERT INTO source (id, name, dept, salary) VALUES (5, 'Eve', 'Sales', 80000)")?;
    Ok(())
}

// ============================================================================
// Test 1: Basic INSERT ... SELECT from another table
// ============================================================================

#[test]
fn test_insert_select_basic() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    // Create target table with same schema
    db.execute(
        "CREATE TABLE target (id INT PRIMARY KEY, name TEXT, dept TEXT, salary INT)"
    )?;

    // INSERT ... SELECT all rows
    let rows = db.execute("INSERT INTO target SELECT * FROM source")?;
    assert_eq!(rows, 5, "Should insert 5 rows");

    // Verify all rows were copied
    let results = db.query("SELECT * FROM target", &[])?;
    assert_eq!(results.len(), 5, "Target table should have 5 rows");

    // Verify specific row data
    let results = db.query("SELECT name, salary FROM target WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(results[0].values[1], Value::Int4(90000));

    Ok(())
}

// ============================================================================
// Test 2: INSERT ... SELECT with WHERE clause
// ============================================================================

#[test]
fn test_insert_select_with_where() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    db.execute(
        "CREATE TABLE eng_team (id INT PRIMARY KEY, name TEXT, dept TEXT, salary INT)"
    )?;

    // Only insert Engineering rows
    let rows = db.execute(
        "INSERT INTO eng_team SELECT * FROM source WHERE dept = 'Engineering'"
    )?;
    assert_eq!(rows, 2, "Should insert 2 Engineering rows");

    let results = db.query("SELECT name FROM eng_team ORDER BY name", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(results[1].values[0], Value::String("Bob".to_string()));

    Ok(())
}

// ============================================================================
// Test 3: INSERT ... SELECT with explicit column list
// ============================================================================

#[test]
fn test_insert_select_with_column_subset() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    // Target table has fewer columns (name and dept only, plus its own id)
    db.execute(
        "CREATE TABLE names_only (name TEXT, dept TEXT)"
    )?;

    // Select only name and dept columns
    let rows = db.execute(
        "INSERT INTO names_only SELECT name, dept FROM source"
    )?;
    assert_eq!(rows, 5, "Should insert 5 rows");

    let results = db.query("SELECT * FROM names_only ORDER BY name", &[])?;
    assert_eq!(results.len(), 5);
    assert_eq!(results[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(results[0].values[1], Value::String("Engineering".to_string()));

    Ok(())
}

// ============================================================================
// Test 4: INSERT ... SELECT with explicit target column list
// ============================================================================

#[test]
fn test_insert_select_with_target_columns() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    db.execute(
        "CREATE TABLE partial_target (id INT, name TEXT, notes TEXT)"
    )?;

    // Insert only id and name, leaving notes as NULL
    let rows = db.execute(
        "INSERT INTO partial_target (id, name) SELECT id, name FROM source WHERE id <= 2"
    )?;
    assert_eq!(rows, 2);

    let results = db.query("SELECT id, name, notes FROM partial_target ORDER BY id", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values[0], Value::Int4(1));
    assert_eq!(results[0].values[1], Value::String("Alice".to_string()));
    assert_eq!(results[0].values[2], Value::Null); // notes should be NULL

    Ok(())
}

// ============================================================================
// Test 5: INSERT ... SELECT with expressions/functions
// ============================================================================

#[test]
fn test_insert_select_with_expressions() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    db.execute(
        "CREATE TABLE raised (id INT, name TEXT, new_salary INT)"
    )?;

    // Apply 10% raise via expression in SELECT
    let rows = db.execute(
        "INSERT INTO raised SELECT id, name, salary + salary / 10 FROM source WHERE dept = 'Engineering'"
    )?;
    assert_eq!(rows, 2);

    let results = db.query("SELECT name, new_salary FROM raised ORDER BY name", &[])?;
    assert_eq!(results.len(), 2);

    // Alice: 90000 + 9000 = 99000
    assert_eq!(results[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(results[0].values[1], Value::Int4(99000));

    // Bob: 85000 + 8500 = 93500
    assert_eq!(results[1].values[0], Value::String("Bob".to_string()));
    assert_eq!(results[1].values[1], Value::Int4(93500));

    Ok(())
}

// ============================================================================
// Test 6: INSERT ... SELECT with aggregates (GROUP BY)
// ============================================================================

#[test]
fn test_insert_select_with_aggregates() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    db.execute(
        "CREATE TABLE dept_summary (dept TEXT, emp_count BIGINT, avg_salary BIGINT)"
    )?;

    // Aggregate by department
    let rows = db.execute(
        "INSERT INTO dept_summary SELECT dept, COUNT(*), AVG(salary) FROM source GROUP BY dept"
    )?;
    assert_eq!(rows, 3, "Should insert 3 department summaries");

    let results = db.query(
        "SELECT dept, emp_count, avg_salary FROM dept_summary ORDER BY dept", &[]
    )?;
    assert_eq!(results.len(), 3);

    // Engineering: 2 employees
    assert_eq!(results[0].values[0], Value::String("Engineering".to_string()));
    assert_eq!(results[0].values[1], Value::Int8(2));

    // Marketing: 2 employees
    assert_eq!(results[1].values[0], Value::String("Marketing".to_string()));
    assert_eq!(results[1].values[1], Value::Int8(2));

    // Sales: 1 employee
    assert_eq!(results[2].values[0], Value::String("Sales".to_string()));
    assert_eq!(results[2].values[1], Value::Int8(1));

    Ok(())
}

// ============================================================================
// Test 7: INSERT ... SELECT column count mismatch (should error)
// ============================================================================

#[test]
fn test_insert_select_column_count_mismatch() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    // Target has 2 columns, source SELECT returns 4
    db.execute("CREATE TABLE narrow (id INT, name TEXT)")?;

    let result = db.execute("INSERT INTO narrow SELECT * FROM source");
    assert!(result.is_err(), "Should fail due to column count mismatch");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("column count mismatch"),
        "Error message should mention column count mismatch, got: {}",
        err_msg
    );

    Ok(())
}

// ============================================================================
// Test 8: INSERT ... SELECT with explicit columns - mismatch
// ============================================================================

#[test]
fn test_insert_select_explicit_column_mismatch() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    db.execute("CREATE TABLE target2 (id INT, name TEXT, dept TEXT)")?;

    // Specify 2 target columns but SELECT returns 3 columns
    let result = db.execute(
        "INSERT INTO target2 (id, name) SELECT id, name, dept FROM source"
    );
    assert!(result.is_err(), "Should fail due to column count mismatch");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("column count mismatch"),
        "Error should mention column count mismatch, got: {}",
        err_msg
    );

    Ok(())
}

// ============================================================================
// Test 9: INSERT ... SELECT from same table (self-referential)
// ============================================================================

#[test]
fn test_insert_select_same_table() -> Result<()> {
    let db = create_test_db()?;

    db.execute("CREATE TABLE items (id INT, name TEXT, category TEXT)")?;
    db.execute("INSERT INTO items VALUES (1, 'Widget', 'A')")?;
    db.execute("INSERT INTO items VALUES (2, 'Gadget', 'B')")?;
    db.execute("INSERT INTO items VALUES (3, 'Thingamajig', 'A')")?;

    // Copy category A items with new IDs (id + 100)
    let rows = db.execute(
        "INSERT INTO items SELECT id + 100, name, category FROM items WHERE category = 'A'"
    )?;
    assert_eq!(rows, 2, "Should insert 2 rows from same table");

    let results = db.query("SELECT * FROM items ORDER BY id", &[])?;
    assert_eq!(results.len(), 5, "Should now have 5 rows total");

    // Check the copied rows
    let results = db.query("SELECT id, name FROM items WHERE id > 100 ORDER BY id", &[])?;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].values[0], Value::Int4(101));
    assert_eq!(results[0].values[1], Value::String("Widget".to_string()));
    assert_eq!(results[1].values[0], Value::Int4(103));
    assert_eq!(results[1].values[1], Value::String("Thingamajig".to_string()));

    Ok(())
}

// ============================================================================
// Test 10: INSERT ... SELECT with ORDER BY and LIMIT
// ============================================================================

#[test]
fn test_insert_select_with_order_limit() -> Result<()> {
    let db = create_test_db()?;
    setup_source_table(&db)?;

    db.execute(
        "CREATE TABLE top_earners (id INT, name TEXT, dept TEXT, salary INT)"
    )?;

    // Insert top 3 earners
    let rows = db.execute(
        "INSERT INTO top_earners SELECT * FROM source ORDER BY salary DESC LIMIT 3"
    )?;
    assert_eq!(rows, 3, "Should insert 3 rows");

    let results = db.query(
        "SELECT name, salary FROM top_earners ORDER BY salary DESC", &[]
    )?;
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(results[0].values[1], Value::Int4(90000));

    Ok(())
}
