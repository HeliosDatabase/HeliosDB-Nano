//! Regression coverage for NANO-DEFICIENCIES A17.
//!
//! The pgvector kNN idiom
//!   `SELECT id, embedding <=> $1 AS d FROM t ORDER BY embedding <=> $1 LIMIT k`
//! returned rows in a non-distance order. Two bugs combined:
//!   1. Sort/TopK built their evaluator without query parameters, so `$1`
//!      errored and the sort key collapsed to NULL.
//!   2. The Sort is planned ABOVE the Project, so the ORDER BY expression
//!      `embedding <=> $1` referenced the base `embedding` column that the
//!      projection had already dropped — it evaluated to an error and the
//!      rows were left unsorted (silently wrong nearest neighbors).
//!
//! The fix threads parameters into Sort/TopK and rewrites an ORDER BY
//! expression that matches a select-list expression to the projected column.
//! These tests use the canonical idiom (the distance is in the select list).

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

fn ids(rows: &[Tuple]) -> Vec<i32> {
    rows.iter()
        .map(|row| match row.values.first() {
            Some(Value::Int4(v)) => *v,
            Some(Value::Int8(v)) => *v as i32,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect()
}

fn seed(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE knn_items (id INT PRIMARY KEY, vec VECTOR(3))")?;
    // L2 distance to the origin query [0,0,0] is the first component: 1..4 for
    // ids 1..4. Insert in a non-distance order so an unsorted result is
    // visibly different from the correct one.
    db.execute("INSERT INTO knn_items VALUES (3, '[3.0,0.0,0.0]')")?;
    db.execute("INSERT INTO knn_items VALUES (1, '[1.0,0.0,0.0]')")?;
    db.execute("INSERT INTO knn_items VALUES (4, '[4.0,0.0,0.0]')")?;
    db.execute("INSERT INTO knn_items VALUES (2, '[2.0,0.0,0.0]')")?;
    Ok(())
}

#[test]
fn knn_orderby_expr_topk_sorts_by_distance() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let q = vec![Value::Vector(vec![0.0, 0.0, 0.0])];

    // Canonical pgvector idiom: distance in the select list, ORDER BY the
    // expression, with a LIMIT (TopK fast path).
    let rows = db.query_params(
        "SELECT id, vec <-> $1 AS d FROM knn_items ORDER BY vec <-> $1 LIMIT 4",
        &q,
    )?;
    assert_eq!(ids(&rows), vec![1, 2, 3, 4]);

    let top2 = db.query_params(
        "SELECT id, vec <-> $1 AS d FROM knn_items ORDER BY vec <-> $1 LIMIT 2",
        &q,
    )?;
    assert_eq!(ids(&top2), vec![1, 2]);
    Ok(())
}

#[test]
fn knn_orderby_expr_full_sort_sorts_by_distance() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let q = vec![Value::Vector(vec![0.0, 0.0, 0.0])];

    // No LIMIT: plain SortOperator path.
    let rows = db.query_params(
        "SELECT id, vec <-> $1 AS d FROM knn_items ORDER BY vec <-> $1",
        &q,
    )?;
    assert_eq!(ids(&rows), vec![1, 2, 3, 4]);
    Ok(())
}

#[test]
fn knn_orderby_expr_matches_alias_form() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let q = vec![Value::Vector(vec![0.0, 0.0, 0.0])];

    // The alias form was the documented workaround; the expr form must agree.
    let expr_form = db.query_params(
        "SELECT id, vec <-> $1 AS d FROM knn_items ORDER BY vec <-> $1 LIMIT 4",
        &q,
    )?;
    let alias_form = db.query_params(
        "SELECT id, vec <-> $1 AS d FROM knn_items ORDER BY d LIMIT 4",
        &q,
    )?;
    assert_eq!(ids(&expr_form), ids(&alias_form));
    assert_eq!(ids(&expr_form), vec![1, 2, 3, 4]);
    Ok(())
}

#[test]
fn knn_orderby_literal_expr_sorts_by_distance() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;

    // Same idiom with a literal query vector (no parameter) — exercises the
    // ORDER-BY-expression → projected-column rewrite independently of params.
    let rows = db.query(
        "SELECT id, vec <-> '[0.0,0.0,0.0]' AS d FROM knn_items ORDER BY vec <-> '[0.0,0.0,0.0]'",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![1, 2, 3, 4]);
    Ok(())
}
