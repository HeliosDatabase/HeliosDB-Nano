//! GitHub #33: `::vector` / `::vector(N)` on arrays and bound parameters.
//!
//! pgvector clients never inline an embedding: they bind it as a parameter,
//! either as the text `'[1,2,3]'` or as a client array. `ARRAY[1.0,2.0,3.0]::vector`
//! and a bare `$1::vector` used to be refused, and a bound text or array value
//! cast with `::vector(3)` failed on the array-literal parser. The cast now takes
//! the text forms `[..]` and `{..}`, SQL arrays and bound parameters of text or
//! array type; a bare `::vector` takes the dimension from the value and
//! `::vector(N)` checks it. Both statement families are pinned.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().unwrap()
}

fn floats(v: &Value) -> Vec<f32> {
    match v {
        Value::Vector(v) => v.clone(),
        other => panic!("not a vector: {other:?}"),
    }
}

fn plain(db: &EmbeddedDatabase, sql: &str) -> Result<Vec<f32>, String> {
    db.query(sql, &[])
        .map(|rows| floats(&rows[0].values[0]))
        .map_err(|e| e.to_string())
}

fn bound(db: &EmbeddedDatabase, sql: &str, p: Value) -> Result<Vec<f32>, String> {
    db.query_params(sql, &[p])
        .map(|rows| floats(&rows[0].values[0]))
        .map_err(|e| e.to_string())
}

fn array(xs: &[f64]) -> Value {
    Value::Array(xs.iter().map(|x| Value::Float8(*x)).collect())
}

#[test]
fn sql_arrays_and_text_cast_without_a_dimension() {
    let db = db();
    let want = vec![1.0, 2.0, 3.0];
    for sql in [
        "SELECT ARRAY[1.0,2.0,3.0]::vector",
        "SELECT ARRAY[1,2,3]::vector",
        "SELECT ARRAY[1.0,2.0,3.0]::vector(3)",
        "SELECT ARRAY[1,2,3]::vector(3)",
        "SELECT '[1,2,3]'::vector",
        "SELECT '{1,2,3}'::vector",
        "SELECT '[1,2,3]'::vector(3)",
        "SELECT CAST(ARRAY[1.0,2.0,3.0] AS vector)",
    ] {
        assert_eq!(plain(&db, sql).unwrap(), want, "{sql}");
    }
    assert!(plain(&db, "SELECT ARRAY[1.0,2.0]::vector(3)")
        .unwrap_err()
        .contains("dimension"));
    assert!(plain(&db, "SELECT '[1,2]'::vector(3)")
        .unwrap_err()
        .contains("dimension"));
}

#[test]
fn bound_parameters_cast_to_vector_in_the_query_family() {
    let db = db();
    let want = vec![1.0, 2.0, 3.0];
    for sql in [
        "SELECT $1::vector",
        "SELECT $1::vector(3)",
        "SELECT CAST($1 AS vector(3))",
    ] {
        for p in [
            Value::String("[1,2,3]".into()),
            Value::String("{1,2,3}".into()),
            array(&[1.0, 2.0, 3.0]),
            Value::Array(vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]),
            Value::Vector(want.clone()),
        ] {
            assert_eq!(bound(&db, sql, p.clone()).unwrap(), want, "{sql} {p:?}");
        }
    }
    let err = bound(&db, "SELECT $1::vector(4)", Value::String("[1,2,3]".into())).unwrap_err();
    assert!(err.contains("dimension"), "{err}");
    let err = bound(&db, "SELECT $1::vector(4)", array(&[1.0, 2.0])).unwrap_err();
    assert!(err.contains("dimension"), "{err}");
    assert!(bound(&db, "SELECT $1::vector", Value::String("nope".into())).is_err());
}

#[test]
fn bound_parameters_insert_and_search_through_the_execute_family() {
    let db = db();
    db.execute("CREATE TABLE mem (id INT PRIMARY KEY, v VECTOR(3))")
        .unwrap();
    db.execute_params(
        "INSERT INTO mem VALUES (1, $1::vector)",
        &[Value::String("[1,0,0]".into())],
    )
    .unwrap();
    db.execute_params("INSERT INTO mem VALUES (2, $1::vector(3))", &[array(&[0.0, 1.0, 0.0])])
        .unwrap();
    db.execute_params(
        "INSERT INTO mem VALUES (3, $1::vector)",
        &[Value::String("{0,0,1}".into())],
    )
    .unwrap();
    // the wrong dimension is refused, not stored
    assert!(db
        .execute_params("INSERT INTO mem VALUES (4, $1::vector(3))", &[array(&[1.0, 2.0])])
        .is_err());
    let rows = db
        .query_params(
            "SELECT id, v <=> $1::vector AS distance FROM mem ORDER BY distance, id LIMIT 2",
            &[array(&[0.0, 1.0, 0.0])],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int4(2));
    // the same through the plain statement family
    let rows = db
        .query(
            "SELECT id FROM mem ORDER BY v <=> ARRAY[0.0,0.0,1.0]::vector, id LIMIT 1",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0].values[0], Value::Int4(3));
}
