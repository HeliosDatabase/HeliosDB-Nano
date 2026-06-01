//! Regression coverage for checklist item A7.
//!
//! UNIQUE TEXT keys must use the full string bytes. Two values that share a
//! 100-character prefix but differ at character 101 are distinct keys.

use heliosdb_nano::storage::ArtIndexManager;
use heliosdb_nano::{EmbeddedDatabase, Result, Value};
use std::collections::HashMap;
use std::time::Instant;

fn shared_prefix_values() -> (String, String) {
    let prefix = "x".repeat(100);
    (format!("{prefix}a"), format!("{prefix}b"))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn count_value(rows: &[heliosdb_nano::Tuple]) -> i64 {
    assert_eq!(rows.len(), 1, "expected a single count row");
    match rows[0].values.first() {
        Some(Value::Int8(value)) => *value,
        Some(Value::Int4(value)) => i64::from(*value),
        other => panic!("expected count value, got {other:?}"),
    }
}

#[test]
fn a7_unique_text_shared_prefix_execute_path() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let (left, right) = shared_prefix_values();

    db.execute("CREATE TABLE a7_execute (id INT PRIMARY KEY, token TEXT NOT NULL UNIQUE)")?;
    db.execute(&format!("INSERT INTO a7_execute VALUES (1, {})", sql_string(&left)))?;
    db.execute(&format!("INSERT INTO a7_execute VALUES (2, {})", sql_string(&right)))?;

    let rows = db.query("SELECT COUNT(*) FROM a7_execute", &[])?;
    assert_eq!(count_value(&rows), 2);

    let duplicate = db.execute(&format!("INSERT INTO a7_execute VALUES (3, {})", sql_string(&left)));
    assert!(duplicate.is_err(), "exact duplicate UNIQUE TEXT value must still fail");
    Ok(())
}

#[test]
fn a7_unique_text_shared_prefix_execute_params_path() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    let (left, right) = shared_prefix_values();

    db.execute("CREATE TABLE a7_params (id INT PRIMARY KEY, token TEXT NOT NULL UNIQUE)")?;
    db.execute_params(
        "INSERT INTO a7_params VALUES ($1, $2)",
        &[Value::Int4(1), Value::String(left.clone())],
    )?;
    db.execute_params(
        "INSERT INTO a7_params VALUES ($1, $2)",
        &[Value::Int4(2), Value::String(right)],
    )?;

    let rows = db.query("SELECT COUNT(*) FROM a7_params", &[])?;
    assert_eq!(count_value(&rows), 2);

    let duplicate = db.execute_params(
        "INSERT INTO a7_params VALUES ($1, $2)",
        &[Value::Int4(3), Value::String(left)],
    );
    assert!(
        duplicate.is_err(),
        "exact duplicate UNIQUE TEXT parameter must still fail"
    );
    Ok(())
}

#[test]
fn a7_art_unique_index_uses_full_text_key() {
    let manager = ArtIndexManager::new();
    manager
        .create_unique_index("a7_art", &["token".to_string()], Some("a7_token_key"))
        .expect("create unique index");

    let (left, right) = shared_prefix_values();
    let left_key = ArtIndexManager::encode_key(&[Value::String(left.clone())]);
    let right_key = ArtIndexManager::encode_key(&[Value::String(right.clone())]);
    assert_ne!(
        left_key, right_key,
        "ART key encoding must include bytes after the 100-char prefix"
    );
    assert_eq!(
        left_key.len(),
        101,
        "single TEXT key should encode the full string bytes"
    );
    assert_eq!(
        right_key.len(),
        101,
        "single TEXT key should encode the full string bytes"
    );

    let mut first = HashMap::new();
    first.insert("token".to_string(), Value::String(left.clone()));
    manager
        .check_unique_constraints("a7_art", &first)
        .expect("first unique value");
    manager
        .on_insert("a7_art", 1, &first)
        .expect("insert first unique value");

    let mut second = HashMap::new();
    second.insert("token".to_string(), Value::String(right));
    manager
        .check_unique_constraints("a7_art", &second)
        .expect("shared-prefix distinct value must not collide");
    manager
        .on_insert("a7_art", 2, &second)
        .expect("insert shared-prefix distinct value");

    let mut duplicate = HashMap::new();
    duplicate.insert("token".to_string(), Value::String(left));
    assert!(
        manager.check_unique_constraints("a7_art", &duplicate).is_err(),
        "exact duplicate TEXT key must still be rejected"
    );
}

#[test]
fn a7_art_unique_index_handles_many_long_shared_prefix_keys() {
    let manager = ArtIndexManager::new();
    manager
        .create_unique_index("a7_art_many", &["token".to_string()], Some("a7_many_token_key"))
        .expect("create unique index");

    let prefix = "y".repeat(100);
    for row_id in 0..100_u64 {
        let mut values = HashMap::new();
        values.insert("token".to_string(), Value::String(format!("{prefix}{row_id:03}")));
        manager
            .check_unique_constraints("a7_art_many", &values)
            .unwrap_or_else(|err| panic!("direct ART unique check failed at row {row_id}: {err}"));
        manager
            .on_insert("a7_art_many", row_id + 1, &values)
            .unwrap_or_else(|err| panic!("direct ART insert failed at row {row_id}: {err}"));
    }
}

#[test]
fn a7_unique_text_shared_prefix_insert_stress() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a7_stress (id INT PRIMARY KEY, token TEXT NOT NULL UNIQUE)")?;

    let prefix = "x".repeat(100);
    let started = Instant::now();
    for id in 0..100 {
        db.execute_params(
            "INSERT INTO a7_stress VALUES ($1, $2)",
            &[Value::Int4(id), Value::String(format!("{prefix}{id:03}"))],
        )
        .map_err(|err| {
            heliosdb_nano::Error::constraint_violation(format!("insert {id} with 100-char shared prefix failed: {err}"))
        })?;
    }
    let elapsed = started.elapsed();

    let rows = db.query("SELECT COUNT(*) FROM a7_stress", &[])?;
    assert_eq!(count_value(&rows), 100);
    eprintln!("A7 stress: 100 shared-prefix UNIQUE TEXT inserts completed in {elapsed:?}");

    Ok(())
}
