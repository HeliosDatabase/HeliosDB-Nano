//! Table function tests for HeliosDB Nano
//!
//! Tests for generate_series() and unnest() table-valued functions.

mod test_helpers;

use heliosdb_nano::{Result, Value};
use test_helpers::*;

// ============================================================================
// generate_series(start, stop) - Basic 2-argument form
// ============================================================================

#[test]
fn test_generate_series_basic() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query("SELECT * FROM generate_series(1, 5)", &[])?;
    assert_eq!(results.len(), 5, "generate_series(1, 5) should produce 5 rows");

    // Verify values are 1, 2, 3, 4, 5
    let values: Vec<i64> = results.iter().map(|t| {
        match &t.values[0] {
            Value::Int8(v) => *v,
            other => panic!("Expected Int8, got {:?}", other),
        }
    }).collect();
    assert_eq!(values, vec![1, 2, 3, 4, 5]);

    Ok(())
}

// ============================================================================
// generate_series(start, stop, step) - 3-argument form with custom step
// ============================================================================

#[test]
fn test_generate_series_with_step() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query("SELECT * FROM generate_series(1, 10, 2)", &[])?;
    assert_eq!(results.len(), 5, "generate_series(1, 10, 2) should produce 5 odd numbers");

    let values: Vec<i64> = results.iter().map(|t| {
        match &t.values[0] {
            Value::Int8(v) => *v,
            other => panic!("Expected Int8, got {:?}", other),
        }
    }).collect();
    assert_eq!(values, vec![1, 3, 5, 7, 9]);

    Ok(())
}

// ============================================================================
// generate_series(start, stop, negative_step) - Descending series
// ============================================================================

#[test]
fn test_generate_series_descending() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query("SELECT * FROM generate_series(5, 1, -1)", &[])?;
    assert_eq!(results.len(), 5, "generate_series(5, 1, -1) should produce 5 rows descending");

    let values: Vec<i64> = results.iter().map(|t| {
        match &t.values[0] {
            Value::Int8(v) => *v,
            other => panic!("Expected Int8, got {:?}", other),
        }
    }).collect();
    assert_eq!(values, vec![5, 4, 3, 2, 1]);

    Ok(())
}

// ============================================================================
// generate_series(n, n) - Single value (start equals stop)
// ============================================================================

#[test]
fn test_generate_series_single_value() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query("SELECT * FROM generate_series(1, 1)", &[])?;
    assert_eq!(results.len(), 1, "generate_series(1, 1) should produce exactly 1 row");

    match &results[0].values[0] {
        Value::Int8(v) => assert_eq!(*v, 1),
        other => panic!("Expected Int8(1), got {:?}", other),
    }

    Ok(())
}

// ============================================================================
// generate_series(5, 1) - Start > stop with default positive step = empty
// ============================================================================

#[test]
fn test_generate_series_empty_wrong_direction() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query("SELECT * FROM generate_series(5, 1)", &[])?;
    assert_eq!(results.len(), 0, "generate_series(5, 1) with default step=1 should produce 0 rows");

    Ok(())
}

// ============================================================================
// generate_series in JOIN: SELECT t.name, g.* FROM table t, generate_series(1, 3) g
// ============================================================================

#[test]
fn test_generate_series_in_cross_join() -> Result<()> {
    let db = create_test_db()?;

    // Create a small table
    db.execute("CREATE TABLE colors (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO colors (id, name) VALUES (1, 'red')")?;
    db.execute("INSERT INTO colors (id, name) VALUES (2, 'blue')")?;

    // Cross join with generate_series using explicit CROSS JOIN
    let results = db.query(
        "SELECT * FROM colors CROSS JOIN generate_series(1, 3) g",
        &[],
    )?;
    // 2 colors x 3 series values = 6 rows
    assert_eq!(results.len(), 6, "Cross join should produce 2 * 3 = 6 rows");

    Ok(())
}

// ============================================================================
// generate_series with WHERE clause
// ============================================================================

#[test]
fn test_generate_series_with_where() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query(
        "SELECT * FROM generate_series(1, 10) WHERE generate_series > 5",
        &[],
    )?;
    assert_eq!(results.len(), 5, "generate_series(1, 10) WHERE > 5 should produce 5 rows");

    let values: Vec<i64> = results.iter().map(|t| {
        match &t.values[0] {
            Value::Int8(v) => *v,
            other => panic!("Expected Int8, got {:?}", other),
        }
    }).collect();
    assert_eq!(values, vec![6, 7, 8, 9, 10]);

    Ok(())
}

// ============================================================================
// generate_series in subquery
// ============================================================================

#[test]
fn test_generate_series_in_subquery() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query(
        "SELECT * FROM (SELECT * FROM generate_series(1, 5)) AS sub",
        &[],
    )?;
    assert_eq!(results.len(), 5, "Subquery with generate_series should produce 5 rows");

    Ok(())
}

// ============================================================================
// generate_series with large step
// ============================================================================

#[test]
fn test_generate_series_large_step() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query("SELECT * FROM generate_series(0, 100, 25)", &[])?;
    assert_eq!(results.len(), 5, "generate_series(0, 100, 25) should produce 5 rows");

    let values: Vec<i64> = results.iter().map(|t| {
        match &t.values[0] {
            Value::Int8(v) => *v,
            other => panic!("Expected Int8, got {:?}", other),
        }
    }).collect();
    assert_eq!(values, vec![0, 25, 50, 75, 100]);

    Ok(())
}

// ============================================================================
// generate_series with negative numbers
// ============================================================================

#[test]
fn test_generate_series_negative_numbers() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query("SELECT * FROM generate_series(-3, 3)", &[])?;
    assert_eq!(results.len(), 7, "generate_series(-3, 3) should produce 7 rows");

    let values: Vec<i64> = results.iter().map(|t| {
        match &t.values[0] {
            Value::Int8(v) => *v,
            other => panic!("Expected Int8, got {:?}", other),
        }
    }).collect();
    assert_eq!(values, vec![-3, -2, -1, 0, 1, 2, 3]);

    Ok(())
}

// ============================================================================
// generate_series with descending step=-2
// ============================================================================

#[test]
fn test_generate_series_descending_step2() -> Result<()> {
    let db = create_test_db()?;
    let results = db.query("SELECT * FROM generate_series(10, 1, -2)", &[])?;
    assert_eq!(results.len(), 5, "generate_series(10, 1, -2) should produce 5 rows");

    let values: Vec<i64> = results.iter().map(|t| {
        match &t.values[0] {
            Value::Int8(v) => *v,
            other => panic!("Expected Int8, got {:?}", other),
        }
    }).collect();
    assert_eq!(values, vec![10, 8, 6, 4, 2]);

    Ok(())
}
