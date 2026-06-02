//! Regression coverage for NANO-DEFICIENCIES A1 (re-tested on 3.35.0).
//!
//! `col = ANY($1)` where the bound parameter is the PostgreSQL array *text*
//! literal (`{a,b,c}`) — the form psycopg and other text-protocol clients send
//! for a Python list — used to error with "ANY expects an array expression,
//! got String". The pre-existing A1 coverage only passed an already-decoded
//! `Value::Array`; this file covers the text form.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

fn string_values(rows: &[Tuple]) -> Vec<String> {
    rows.iter()
        .map(|row| match row.values.first() {
            Some(Value::String(value)) => value.clone(),
            other => panic!("expected string value, got {other:?}"),
        })
        .collect()
}

fn int_values(rows: &[Tuple]) -> Vec<i32> {
    rows.iter()
        .map(|row| match row.values.first() {
            Some(Value::Int4(v)) => *v,
            Some(Value::Int8(v)) => *v as i32,
            other => panic!("expected int value, got {other:?}"),
        })
        .collect()
}

fn seed_text(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE a1t_items (id INT PRIMARY KEY, ext TEXT)")?;
    db.execute("INSERT INTO a1t_items VALUES (1, 'pdf'), (2, 'docx'), (3, 'pptx'), (4, 'png')")?;
    Ok(())
}

#[test]
fn any_text_array_literal_param_matches_in_list() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_text(&db)?;

    let expected = db.query(
        "SELECT ext FROM a1t_items WHERE ext IN ('pdf','docx','pptx') ORDER BY ext",
        &[],
    )?;

    // The exact byte form psycopg sends for a Python list parameter.
    let via_text_param = db.query_params(
        "SELECT ext FROM a1t_items WHERE ext = ANY($1) ORDER BY ext",
        &[Value::String("{pdf,docx,pptx}".to_string())],
    )?;
    // …and with the explicit ::text[] cast the deficiency report tried.
    let via_text_param_cast = db.query_params(
        "SELECT ext FROM a1t_items WHERE ext = ANY($1::text[]) ORDER BY ext",
        &[Value::String("{pdf,docx,pptx}".to_string())],
    )?;

    assert_eq!(string_values(&via_text_param), string_values(&expected));
    assert_eq!(string_values(&via_text_param_cast), string_values(&expected));
    Ok(())
}

#[test]
fn any_text_array_param_quoted_elements() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a1t_q (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO a1t_q VALUES (1, 'a,b'), (2, 'plain'), (3, 'c d')")?;

    // Quoted elements with embedded comma / space.
    let rows = db.query_params(
        "SELECT name FROM a1t_q WHERE name = ANY($1) ORDER BY id",
        &[Value::String("{\"a,b\",\"c d\"}".to_string())],
    )?;
    assert_eq!(string_values(&rows), vec!["a,b".to_string(), "c d".to_string()]);
    Ok(())
}

#[test]
fn any_text_array_param_coerces_to_int_column() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a1t_nums (id INT PRIMARY KEY, g INT)")?;
    db.execute("INSERT INTO a1t_nums VALUES (1, 10), (2, 20), (3, 30), (4, 40)")?;

    // Text array elements compared against an INT column rely on values_equal
    // String↔Int coercion.
    let rows = db.query_params(
        "SELECT id FROM a1t_nums WHERE g = ANY($1) ORDER BY id",
        &[Value::String("{10,30}".to_string())],
    )?;
    assert_eq!(int_values(&rows), vec![1, 3]);
    Ok(())
}

#[test]
fn any_empty_text_array_param_matches_nothing() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed_text(&db)?;

    let rows = db.query_params(
        "SELECT ext FROM a1t_items WHERE ext = ANY($1)",
        &[Value::String("{}".to_string())],
    )?;
    assert!(rows.is_empty(), "empty array parameter must match no rows");
    Ok(())
}
