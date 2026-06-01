//! Regression for checklist item **T8** — "GROUP-BY-without-aggregate MV does not
//! dedupe rows."
//!
//! Acceptance: `CREATE MATERIALIZED VIEW mv AS SELECT a, b FROM t GROUP BY a, b`
//! materializes DISTINCT (a, b) rows — i.e. `COUNT(*) FROM mv` equals
//! `COUNT(DISTINCT a, b)` of the base, not the base row count.

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

fn scalar_i64(rows: &[Tuple]) -> i64 {
    match rows.first().and_then(|t| t.values.first()) {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        other => panic!("unexpected scalar: {other:?}"),
    }
}

#[test]
fn t8_groupby_without_aggregate_mv_dedupes() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, a TEXT, b INT)")
        .expect("create base");
    // 6 base rows; 3 distinct (a,b) pairs: (x,1)x3, (y,2)x2, (z,3)x1.
    db.execute("INSERT INTO t VALUES (1,'x',1),(2,'x',1),(3,'x',1),(4,'y',2),(5,'y',2),(6,'z',3)")
        .expect("seed");
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT a, b FROM t GROUP BY a, b")
        .expect("create mv");

    let mv_scan = db.query("SELECT a, b FROM mv", &[]).expect("scan mv").len() as i64;
    let mv_count = scalar_i64(&db.query("SELECT COUNT(*) FROM mv", &[]).expect("count mv"));

    assert_eq!(
        mv_scan, 3,
        "GROUP BY a,b without aggregates must materialize 3 DISTINCT rows, got {mv_scan}"
    );
    assert_eq!(
        mv_count, 3,
        "COUNT(*) over the GROUP-BY MV must be 3 (distinct pairs), got {mv_count}"
    );

    // The materialized rows must be exactly the 3 distinct pairs.
    let mut pairs: Vec<(String, i64)> = db
        .query("SELECT a, b FROM mv", &[])
        .expect("scan mv pairs")
        .iter()
        .map(|t| {
            let a = match &t.values[0] {
                Value::String(s) => s.clone(),
                other => panic!("a not text: {other:?}"),
            };
            let b = match &t.values[1] {
                Value::Int4(n) => i64::from(*n),
                Value::Int8(n) => *n,
                other => panic!("b not int: {other:?}"),
            };
            (a, b)
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![("x".to_string(), 1), ("y".to_string(), 2), ("z".to_string(), 3)],
        "MV must contain exactly the distinct (a,b) pairs"
    );
}
