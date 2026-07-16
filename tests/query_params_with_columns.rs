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

/// W1.1: `query_params_with_columns` and `query_params` skip the
/// `current_transaction` mutex on autocommit reads via the `global_txn_active`
/// fast-out. The load-bearing correctness property is that the gated lock still
/// engages when a transaction IS open — so a parameterized read inside an
/// explicit transaction must see that transaction's uncommitted write set, and
/// an autocommit read after COMMIT/ROLLBACK must reflect the committed state.
#[test]
fn param_read_sees_open_txn_write_set_and_committed_state() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1,'a')").unwrap();

    let a = || Value::String("a".into());
    let ids_cols = |db: &EmbeddedDatabase| -> Vec<i64> {
        db.query_params_with_columns("SELECT id FROM t WHERE name = $1 ORDER BY id", &[a()])
            .unwrap()
            .0
            .iter()
            .map(|r| as_i64(&r.values[0]))
            .collect()
    };
    let ids_plan = |db: &EmbeddedDatabase| -> Vec<i64> {
        db.query_params("SELECT id FROM t WHERE name = $1 ORDER BY id", &[a()])
            .unwrap()
            .iter()
            .map(|r| as_i64(&r.values[0]))
            .collect()
    };

    // Autocommit: no global txn → lock-free fast-out on both entry points.
    assert_eq!(ids_cols(&db), vec![1]);
    assert_eq!(ids_plan(&db), vec![1]);

    // Open a txn and stage a write: `global_txn_active` is true, so the gated
    // lock must engage and the read must see the uncommitted write-set row.
    db.begin().unwrap();
    db.execute("INSERT INTO t VALUES (2,'a')").unwrap();
    assert_eq!(ids_cols(&db), vec![1, 2], "column path: in-txn read must see write set");
    assert_eq!(ids_plan(&db), vec![1, 2], "plan path: in-txn read must see write set");

    // Rollback drops the staged row; autocommit reads are lock-free again.
    db.rollback().unwrap();
    assert_eq!(ids_cols(&db), vec![1], "column path: write set gone after rollback");
    assert_eq!(ids_plan(&db), vec![1], "plan path: write set gone after rollback");

    // Re-stage and COMMIT; autocommit reads see the committed row.
    db.begin().unwrap();
    db.execute("INSERT INTO t VALUES (3,'a')").unwrap();
    db.commit().unwrap();
    assert_eq!(ids_cols(&db), vec![1, 3], "column path: committed row visible");
    assert_eq!(ids_plan(&db), vec![1, 3], "plan path: committed row visible");
}

/// W1.4: a normalized `WHERE c.id = $1` feeding a join must return the SAME
/// rows as its literal twin (now routed through index-nested-loop instead of a
/// hash join), and a NULL parameter on the pushed predicate must match nothing
/// (SQL three-valued logic — `col = NULL` is UNKNOWN → zero rows).
#[test]
fn param_equality_join_matches_literal_and_null_matches_nothing() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, total INT)")
        .unwrap();
    db.execute("CREATE INDEX idx_orders_customer ON orders(customer_id)")
        .unwrap();
    db.execute("INSERT INTO customers VALUES (1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    db.execute("INSERT INTO orders VALUES (10,1,100),(11,1,200),(12,2,300)")
        .unwrap();

    let sql = "SELECT o.id FROM customers c JOIN orders o ON o.customer_id = c.id \
               WHERE c.id = $1 ORDER BY o.id";
    let ids = |rows: &[heliosdb_nano::Tuple]| -> Vec<i64> { rows.iter().map(|r| as_i64(&r.values[0])).collect() };

    // Literal twin vs parameterized: row-for-row identical.
    let literal = db
        .query(
            "SELECT o.id FROM customers c JOIN orders o ON o.customer_id = c.id \
             WHERE c.id = 1 ORDER BY o.id",
            &[],
        )
        .unwrap();
    let param = db.query_params(sql, &[Value::Int4(1)]).unwrap();
    assert_eq!(ids(&param), ids(&literal), "param join must equal literal join");
    assert_eq!(ids(&param), vec![10, 11]);

    // NULL parameter on the pushed equality → zero rows.
    let null_param = db.query_params(sql, &[Value::Null]).unwrap();
    assert!(
        null_param.is_empty(),
        "NULL param equality must match nothing, got {:?}",
        ids(&null_param)
    );
}

/// W1.4 correctness guard for the non-indexed FilteredScan path: widening the
/// pushdown gate lets `WHERE note = $1` (note is NOT indexed) become a
/// FilteredScan whose predicate resolves to a runtime value. A NULL parameter
/// must still yield zero rows even though rows with `note IS NULL` exist — the
/// storage-safety gate defers the NULL bound to the executor's NULL-correct
/// evaluator instead of the SIMD kernel (which would match `NULL = NULL`).
#[test]
fn param_equality_null_does_not_match_null_column_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t2 (id INT PRIMARY KEY, note TEXT)").unwrap();
    db.execute("INSERT INTO t2 VALUES (1,'x'),(2,NULL),(3,'x'),(4,NULL)")
        .unwrap();

    let sql = "SELECT id FROM t2 WHERE note = $1 ORDER BY id";
    let ids = |rows: &[heliosdb_nano::Tuple]| -> Vec<i64> { rows.iter().map(|r| as_i64(&r.values[0])).collect() };

    // Non-null param still filters correctly through the widened FilteredScan.
    let matched = db.query_params(sql, &[Value::String("x".into())]).unwrap();
    assert_eq!(ids(&matched), vec![1, 3]);

    // NULL param must NOT return the `note IS NULL` rows (2 and 4).
    let null_param = db.query_params(sql, &[Value::Null]).unwrap();
    assert!(
        null_param.is_empty(),
        "NULL param must not match NULL-column rows, got {:?}",
        ids(&null_param)
    );
}

/// W1.4 review guard: widening the pushdown gate to accept `(Column, Parameter)`
/// must not let an out-of-range `$n` silently drop the conjunct and return every
/// row. `WHERE note = $2` with a single bound param (note is NOT indexed → a
/// FilteredScan on the non-indexed path) resolves nothing at runtime, so
/// `analyze_predicate` drops the conjunct and `analyzed_predicates` is shorter
/// than the predicate. The completeness check treats that pushdown as unsafe and
/// the executor re-filter reproduces the pre-widening `Parameter $2 not provided`
/// error instead of returning the whole table unfiltered.
#[test]
fn param_out_of_range_on_pushed_predicate_errors_not_all_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t2 (id INT PRIMARY KEY, note TEXT)").unwrap();
    db.execute("INSERT INTO t2 VALUES (1,'x'),(2,NULL),(3,'x'),(4,NULL)")
        .unwrap();

    // User meant $1 but wrote $2 and passed a single param. On the pre-fix tree
    // this became a FilteredScan whose $2 resolved to None, dropping the conjunct
    // and returning ALL rows; the correct behavior is the missing-parameter error.
    let result = db.query_params("SELECT id FROM t2 WHERE note = $2", &[Value::String("x".into())]);
    assert!(
        result.is_err(),
        "out-of-range $2 must error, not return unfiltered rows: {:?}",
        result.map(|rows| rows.iter().map(|r| as_i64(&r.values[0])).collect::<Vec<_>>())
    );
}
