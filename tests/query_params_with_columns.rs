//! Regression for `EmbeddedDatabase::query_params_with_columns` — the column-aware,
//! parameter-binding query entry point added for the PyO3 binding (issue #1). It must
//! return both rows and output column names, behave like `query_with_columns` with an
//! empty param slice, and bind `$n` positionally otherwise.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int8(n) => *n,
        Value::Int4(n) => i64::from(*n),
        Value::Int2(n) => i64::from(*n),
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn columns_and_param_binding() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'a')").unwrap();

    // Empty params == query_with_columns: all rows, with column names.
    let (rows, cols) = db
        .query_params_with_columns("SELECT id, name FROM t ORDER BY id", &[])
        .unwrap();
    assert_eq!(cols, vec!["id".to_string(), "name".to_string()]);
    assert_eq!(rows.len(), 3);
    assert_eq!(as_i64(&rows[0].values[0]), 1);

    // $1 binds positionally.
    let (rows, cols) = db
        .query_params_with_columns(
            "SELECT id FROM t WHERE name = $1 ORDER BY id",
            &[Value::String("a".into())],
        )
        .unwrap();
    assert_eq!(cols, vec!["id".to_string()]);
    let ids: Vec<i64> = rows.iter().map(|r| as_i64(&r.values[0])).collect();
    assert_eq!(ids, vec![1, 3]);

    // Aggregate keeps its alias as the column name.
    let (rows, cols) = db
        .query_params_with_columns(
            "SELECT COUNT(*) AS n FROM t WHERE name = $1",
            &[Value::String("a".into())],
        )
        .unwrap();
    assert_eq!(cols, vec!["n".to_string()]);
    assert_eq!(as_i64(&rows[0].values[0]), 2);
}
