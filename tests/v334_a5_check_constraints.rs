//! Regression coverage for checklist item A5.
//!
//! CHECK constraints are parsed and stored during CREATE TABLE. This pins that
//! INSERT and UPDATE write paths, including parameterized forms that may be
//! eligible for fast paths, reject rows whose CHECK expression evaluates false.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};
use std::time::Instant;

fn int_col(db: &EmbeddedDatabase, table: &str, col: &str, id: i32) -> Result<i32> {
    let rows = db.query(&format!("SELECT {col} FROM {table} WHERE id = {id}"), &[])?;
    assert_eq!(rows.len(), 1, "expected one row for id={id}");
    match rows[0].values.first() {
        Some(Value::Int4(value)) => Ok(*value),
        other => panic!("expected int4 {col}, got {other:?}"),
    }
}

fn count_rows(db: &EmbeddedDatabase, table: &str) -> Result<usize> {
    Ok(db.query(&format!("SELECT id FROM {table}"), &[])?.len())
}

#[test]
fn a5_check_constraints_reject_invalid_parameterized_insert_and_update() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE a5_scores (
            id INT PRIMARY KEY,
            score INT CHECK (score >= 0 AND score <= 100),
            attempts INT,
            CHECK (attempts >= 0)
        )",
    )?;

    db.execute("INSERT INTO a5_scores VALUES (1, 50, 1)")?;
    db.execute_params(
        "INSERT INTO a5_scores VALUES ($1, $2, $3)",
        &[Value::Int4(2), Value::Int4(0), Value::Int4(3)],
    )?;

    let invalid_literal_insert = db.execute("INSERT INTO a5_scores VALUES (3, -1, 1)");
    assert!(
        invalid_literal_insert.is_err(),
        "literal INSERT violating column CHECK must fail"
    );

    let invalid_param_insert = db.execute_params(
        "INSERT INTO a5_scores VALUES ($1, $2, $3)",
        &[Value::Int4(4), Value::Int4(25), Value::Int4(-1)],
    );
    assert!(
        invalid_param_insert.is_err(),
        "parameterized INSERT violating table CHECK must fail"
    );

    let valid_update = db.execute_params(
        "UPDATE a5_scores SET score = $1 WHERE id = $2",
        &[Value::Int4(75), Value::Int4(1)],
    )?;
    assert_eq!(valid_update, 1);
    assert_eq!(int_col(&db, "a5_scores", "score", 1)?, 75);

    let invalid_literal_update = db.execute("UPDATE a5_scores SET score = 101 WHERE id = 1");
    assert!(
        invalid_literal_update.is_err(),
        "literal UPDATE violating CHECK must fail"
    );
    assert_eq!(
        int_col(&db, "a5_scores", "score", 1)?,
        75,
        "failed UPDATE must leave the old row intact"
    );

    let invalid_param_update = db.execute_params(
        "UPDATE a5_scores SET attempts = $1 WHERE id = $2",
        &[Value::Int4(-2), Value::Int4(2)],
    );
    assert!(
        invalid_param_update.is_err(),
        "parameterized UPDATE violating CHECK must fail"
    );
    assert_eq!(count_rows(&db, "a5_scores")?, 2);

    Ok(())
}

#[test]
fn a5_table_level_check_rejects_invalid_writes() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        "CREATE TABLE a5_ranges (
            id INT PRIMARY KEY,
            lo INT,
            hi INT,
            CHECK (lo <= hi)
        )",
    )?;

    db.execute_params(
        "INSERT INTO a5_ranges VALUES ($1, $2, $3)",
        &[Value::Int4(1), Value::Int4(10), Value::Int4(20)],
    )?;

    let invalid_insert = db.execute_params(
        "INSERT INTO a5_ranges VALUES ($1, $2, $3)",
        &[Value::Int4(2), Value::Int4(30), Value::Int4(20)],
    );
    assert!(
        invalid_insert.is_err(),
        "parameterized INSERT violating table-level CHECK must fail"
    );

    let invalid_update = db.execute_params(
        "UPDATE a5_ranges SET lo = $1 WHERE id = $2",
        &[Value::Int4(25), Value::Int4(1)],
    );
    assert!(
        invalid_update.is_err(),
        "parameterized UPDATE violating table-level CHECK must fail"
    );
    assert_eq!(int_col(&db, "a5_ranges", "lo", 1)?, 10);
    assert_eq!(count_rows(&db, "a5_ranges")?, 1);

    Ok(())
}

#[test]
fn a5_secondary_write_paths_reject_check_violations() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a5_source (id INT PRIMARY KEY, balance INT)")?;
    db.execute("CREATE TABLE a5_accounts (id INT PRIMARY KEY, balance INT CHECK (balance >= 0))")?;

    db.execute("INSERT INTO a5_source VALUES (1, 10)")?;
    db.execute_params(
        "INSERT INTO a5_source VALUES ($1, $2)",
        &[Value::Int4(2), Value::Int4(-5)],
    )?;

    let insert_select = db.execute("INSERT INTO a5_accounts SELECT id, balance FROM a5_source");
    assert!(insert_select.is_err(), "INSERT ... SELECT violating CHECK must fail");
    assert_eq!(
        db.query("SELECT id FROM a5_accounts WHERE balance < 0", &[])?.len(),
        0,
        "failed INSERT ... SELECT must not leak CHECK-violating rows"
    );

    db.execute("DELETE FROM a5_accounts")?;
    db.execute_params(
        "INSERT INTO a5_accounts VALUES ($1, $2)",
        &[Value::Int4(1), Value::Int4(10)],
    )?;
    let invalid_upsert = db.execute_params(
        "INSERT INTO a5_accounts VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET balance = $3",
        &[Value::Int4(1), Value::Int4(11), Value::Int4(-9)],
    );
    assert!(
        invalid_upsert.is_err(),
        "ON CONFLICT DO UPDATE violating CHECK must fail"
    );
    assert_eq!(int_col(&db, "a5_accounts", "balance", 1)?, 10);

    Ok(())
}

#[test]
fn a5_check_constraint_rejection_stress() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a5_stress (id INT PRIMARY KEY, score INT CHECK (score >= 0))")?;

    let started = Instant::now();
    for id in 0..200 {
        db.execute_params(
            "INSERT INTO a5_stress VALUES ($1, $2)",
            &[Value::Int4(id), Value::Int4(id)],
        )?;
        let rejected = db.execute_params(
            "UPDATE a5_stress SET score = $1 WHERE id = $2",
            &[Value::Int4(-1), Value::Int4(id)],
        );
        assert!(rejected.is_err(), "CHECK-violating update {id} must fail");
    }
    let elapsed = started.elapsed();
    eprintln!("A5 stress: 200 accepted inserts + 200 rejected updates completed in {elapsed:?}");

    assert_eq!(count_rows(&db, "a5_stress")?, 200);
    assert_eq!(int_col(&db, "a5_stress", "score", 199)?, 199);

    Ok(())
}
