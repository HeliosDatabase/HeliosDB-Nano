#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
//! v01.10: `x op ANY / ALL (array)` follows PostgreSQL — array columns, array
//! text literals and the comparison operator — on every route: autocommit,
//! inside BEGIN … COMMIT, and with the array as a bound parameter.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE ar (id INT PRIMARY KEY, tags TEXT[], nums BIGINT[])")
        .expect("create");
    db.execute("INSERT INTO ar VALUES (1, '{a,b}', '{1,2}')")
        .expect("insert 1");
    db.execute("INSERT INTO ar VALUES (2, '{x,y}', '{10,20}')")
        .expect("insert 2");
    db.execute("INSERT INTO ar VALUES (3, '{z,NULL}', '{30,NULL}')")
        .expect("insert 3");
    db
}

fn ids(rows: &[heliosdb_nano::Tuple]) -> Vec<i64> {
    let mut out: Vec<i64> = rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Int4(v) => i64::from(*v),
            Value::Int8(v) => *v,
            other => panic!("id {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

/// Each statement in autocommit, in a transaction, and (when given) with the
/// array bound as `$1`; all must agree with `want`.
fn check_ids(db: &EmbeddedDatabase, sql: &str, want: &[i64]) {
    let auto = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(ids(&auto), want, "autocommit: {sql}");
    db.execute("BEGIN").expect("begin");
    let txn = db.query(sql, &[]).unwrap_or_else(|e| panic!("txn {sql}: {e}"));
    db.execute("COMMIT").expect("commit");
    assert_eq!(ids(&txn), want, "in transaction: {sql}");
}

fn scalar(db: &EmbeddedDatabase, sql: &str) -> Value {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let auto = rows[0].values[0].clone();
    db.execute("BEGIN").expect("begin");
    let txn = db.query(sql, &[]).unwrap_or_else(|e| panic!("txn {sql}: {e}"))[0].values[0].clone();
    db.execute("COMMIT").expect("commit");
    assert_eq!(auto, txn, "{sql}: autocommit and transaction differ");
    auto
}

#[test]
fn any_over_array_columns_finds_the_rows() {
    let db = db();
    check_ids(&db, "SELECT id FROM ar WHERE 'x' = ANY(tags)", &[2]);
    check_ids(&db, "SELECT id FROM ar WHERE 10 = ANY(nums)", &[2]);
    check_ids(&db, "SELECT id FROM ar WHERE 'q' = ANY(tags)", &[]);
    check_ids(&db, "SELECT id FROM ar WHERE 15 < ANY(nums)", &[2, 3]);
    check_ids(&db, "SELECT id FROM ar WHERE 5 > ALL(nums)", &[1]);
    check_ids(&db, "SELECT id FROM ar WHERE 'a' <> ANY(tags)", &[1, 2, 3]);
}

#[test]
fn a_null_element_makes_a_failed_any_unknown() {
    let db = db();
    let rows = db
        .query(
            "SELECT id, 'q' = ANY(tags), 99 = ANY(nums), 30 = ANY(nums) FROM ar ORDER BY id",
            &[],
        )
        .expect("select");
    assert_eq!(rows[0].values[1], Value::Boolean(false));
    assert_eq!(rows[2].values[1], Value::Null, "no match beside a NULL element is NULL");
    assert_eq!(rows[2].values[2], Value::Null);
    assert_eq!(rows[2].values[3], Value::Boolean(true));
}

#[test]
fn the_comparison_operator_is_honoured() {
    let db = db();
    assert_eq!(scalar(&db, "SELECT 1 <> ANY(ARRAY[1,2])"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 3 > ANY(ARRAY[1,2])"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 1 > ANY(ARRAY[1,2])"), Value::Boolean(false));
    assert_eq!(scalar(&db, "SELECT 1 <> ANY(ARRAY[1,1])"), Value::Boolean(false));
    assert_eq!(scalar(&db, "SELECT 2 >= ALL(ARRAY[1,2])"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 1 = ALL(ARRAY[1,2])"), Value::Boolean(false));
    assert_eq!(scalar(&db, "SELECT 1 <> ALL(ARRAY[2,3])"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 1 < ANY(ARRAY[NULL, 0])"), Value::Null);
    assert_eq!(scalar(&db, "SELECT 2 = ANY('{2,3}')"), Value::Boolean(true));
    assert_eq!(scalar(&db, "SELECT 4 = ANY('{2,3}')"), Value::Boolean(false));
    assert_eq!(scalar(&db, "SELECT 2 < ANY('{2,3}')"), Value::Boolean(true));
    check_ids(&db, "SELECT id FROM ar WHERE id = ANY('{2,3}')", &[2, 3]);
    check_ids(&db, "SELECT id FROM ar WHERE id <> ANY(ARRAY[1,2])", &[1, 2, 3]);
    check_ids(&db, "SELECT id FROM ar WHERE id <> ALL(ARRAY[1,2])", &[3]);
    check_ids(&db, "SELECT id FROM ar WHERE id > ALL('{1,2}')", &[3]);
}

#[test]
fn a_bound_array_parameter_gives_the_same_answers() {
    let db = db();
    let text = |s: &str| Value::String(s.to_string());
    let arr = |v: &[i64]| Value::Array(v.iter().map(|n| Value::Int8(*n)).collect());
    for (sql, param, want) in [
        ("SELECT id FROM ar WHERE id = ANY($1)", text("{2,3}"), vec![2, 3]),
        ("SELECT id FROM ar WHERE id = ANY($1)", arr(&[2, 3]), vec![2, 3]),
        ("SELECT id FROM ar WHERE id > ANY($1)", arr(&[1, 2]), vec![2, 3]),
        ("SELECT id FROM ar WHERE id <> ALL($1)", text("{1,2}"), vec![3]),
        ("SELECT id FROM ar WHERE id < ALL($1)", arr(&[3]), vec![1, 2]),
    ] {
        let rows = db
            .query_params(sql, std::slice::from_ref(&param))
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(ids(&rows), want, "{sql} with {param:?}");
    }
}

#[test]
fn an_unplannable_quantified_form_is_an_error_not_a_constant() {
    let db = db();
    let err = db
        .query("SELECT id FROM ar WHERE id < ANY(SELECT id FROM ar)", &[])
        .expect_err("refused");
    assert!(err.to_string().contains("not supported"), "{err}");
    // `= ANY (subquery)` is IN (subquery).
    check_ids(
        &db,
        "SELECT id FROM ar WHERE id = ANY(SELECT id FROM ar WHERE id > 1)",
        &[2, 3],
    );
}
