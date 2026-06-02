//! Regression coverage for NANO-DEFICIENCIES A17.
//!
//! `ORDER BY <vector-distance-expr>` where the query vector is a bound
//! parameter (`ORDER BY embedding <=> $1`) used to return rows in a
//! non-distance order: the Sort/TopK operators built their evaluator without
//! the query parameters, so `$1` resolved to an error, every sort key
//! collapsed to NULL, and the rows came back unsorted (silently wrong nearest
//! neighbors). The expr-form must now match the alias-form ordering.

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
    // Distances to the origin query [0,0,0] are 1,2,3,4 for ids 1..4.
    // Insert in a deliberately non-distance order so an unsorted result
    // (the pre-fix behavior) is visibly different from the correct one.
    db.execute("INSERT INTO knn_items VALUES (3, '[3.0,0.0,0.0]')")?;
    db.execute("INSERT INTO knn_items VALUES (1, '[1.0,0.0,0.0]')")?;
    db.execute("INSERT INTO knn_items VALUES (4, '[4.0,0.0,0.0]')")?;
    db.execute("INSERT INTO knn_items VALUES (2, '[2.0,0.0,0.0]')")?;
    Ok(())
}

#[test]
fn knn_orderby_param_expr_sorts_by_distance_topk() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let q = vec![Value::Vector(vec![0.0, 0.0, 0.0])];

    // Expr-form ORDER BY with a LIMIT exercises the TopK fast path.
    let rows = db.query_params(
        "SELECT id FROM knn_items ORDER BY vec <-> $1 LIMIT 4",
        &q,
    )?;
    assert_eq!(ids(&rows), vec![1, 2, 3, 4]);

    // A tighter LIMIT (true top-k) must still return the two nearest in order.
    let top2 = db.query_params(
        "SELECT id FROM knn_items ORDER BY vec <-> $1 LIMIT 2",
        &q,
    )?;
    assert_eq!(ids(&top2), vec![1, 2]);
    Ok(())
}

#[test]
fn knn_orderby_param_expr_sorts_by_distance_full_sort() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let q = vec![Value::Vector(vec![0.0, 0.0, 0.0])];

    // No LIMIT exercises the plain SortOperator path.
    let rows = db.query_params("SELECT id FROM knn_items ORDER BY vec <-> $1", &q)?;
    assert_eq!(ids(&rows), vec![1, 2, 3, 4]);
    Ok(())
}

#[test]
fn knn_orderby_param_expr_matches_alias_form() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let q = vec![Value::Vector(vec![0.0, 0.0, 0.0])];

    // The alias-form was the documented workaround; the expr-form must agree.
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
