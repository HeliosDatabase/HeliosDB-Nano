//! Regression coverage for checklist item A1.
//!
//! `col = ANY($1)` with `$1` bound to an array should match the same rows as
//! `col IN (...)`. The planner used to handle only one casted literal-array
//! form and silently rewrote parameter arrays to constant false.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};
use std::time::Instant;

fn string_values(rows: &[Tuple]) -> Vec<String> {
    rows.iter()
        .map(|row| match row.values.first() {
            Some(Value::String(value)) => value.clone(),
            other => panic!("expected string value, got {other:?}"),
        })
        .collect()
}

fn count_value(rows: &[Tuple]) -> i64 {
    assert_eq!(rows.len(), 1, "expected a single count row");
    match rows[0].values.first() {
        Some(Value::Int8(value)) => *value,
        Some(Value::Int4(value)) => i64::from(*value),
        other => panic!("expected count value, got {other:?}"),
    }
}

fn seed_text_table(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE a1_items (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO a1_items VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma')")?;
    Ok(())
}

#[test]
fn a1_any_parameter_array_matches_in_list_for_text() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_text_table(&db)?;

    let expected = db.query(
        "SELECT name FROM a1_items WHERE name IN ('alpha', 'beta') ORDER BY name",
        &[],
    )?;
    let actual = db.query_params(
        "SELECT name FROM a1_items WHERE name = ANY($1) ORDER BY name",
        &[Value::Array(vec![
            Value::String("alpha".to_string()),
            Value::String("beta".to_string()),
        ])],
    )?;
    let actual_casted = db.query_params(
        "SELECT name FROM a1_items WHERE name = ANY($1::text[]) ORDER BY name",
        &[Value::Array(vec![
            Value::String("alpha".to_string()),
            Value::String("beta".to_string()),
        ])],
    )?;

    assert_eq!(string_values(&actual), string_values(&expected));
    assert_eq!(string_values(&actual_casted), string_values(&expected));
    Ok(())
}

#[test]
fn a1_any_literal_array_matches_in_list_for_text() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_text_table(&db)?;

    let rows = db.query(
        "SELECT name FROM a1_items WHERE name = ANY(ARRAY['alpha', 'beta']) ORDER BY name",
        &[],
    )?;
    let cast_rows = db.query(
        "SELECT name FROM a1_items WHERE name = ANY('{alpha,beta}'::text[]) ORDER BY name",
        &[],
    )?;

    assert_eq!(string_values(&rows), vec!["alpha".to_string(), "beta".to_string()]);
    assert_eq!(string_values(&cast_rows), vec!["alpha".to_string(), "beta".to_string()]);
    Ok(())
}

#[test]
fn a1_plain_in_parameter_array_is_not_rewritten_as_any() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_text_table(&db)?;

    let rows = db.query_params(
        "SELECT name FROM a1_items WHERE name IN ($1) ORDER BY name",
        &[Value::Array(vec![
            Value::String("alpha".to_string()),
            Value::String("beta".to_string()),
        ])],
    )?;

    assert!(
        rows.is_empty(),
        "plain IN($1) with an array parameter must not be expanded as ANY($1)"
    );
    Ok(())
}

#[test]
fn a1_any_parameter_array_rejection_stress() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a1_numbers (id INT PRIMARY KEY, group_id INT)")?;
    for id in 0..200 {
        db.execute_params(
            "INSERT INTO a1_numbers VALUES ($1, $2)",
            &[Value::Int4(id), Value::Int4(id % 10)],
        )?;
    }

    let started = Instant::now();
    for group_id in 0..10 {
        let rows = db.query_params(
            "SELECT COUNT(*) FROM a1_numbers WHERE group_id = ANY($1)",
            &[Value::Array(vec![Value::Int4(group_id)])],
        )?;
        assert_eq!(count_value(&rows), 20);
    }
    let elapsed = started.elapsed();
    eprintln!("A1 stress: 10 parameter-array ANY count queries completed in {elapsed:?}");

    Ok(())
}
