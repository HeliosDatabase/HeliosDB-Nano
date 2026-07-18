//! A2 integration: `query_with_columns` (the wire read path) returns correct
//! results when literal normalization is active (the default), across many
//! literal variants of the same query shape. The unit-level differential oracle
//! (src/sql/normalize.rs `differential`) proves raw==normalized directly; this
//! confirms the wired path end-to-end.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn seed() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t50 (aid INT, abalance INT, name TEXT)")
        .unwrap();
    db.execute("CREATE INDEX t50_aid ON t50(aid)").unwrap();
    for i in 1..=50 {
        db.execute(&format!("INSERT INTO t50 VALUES ({i}, {}, 'n{i}')", (i * 7) % 100000))
            .unwrap();
    }
    db
}

/// The pgbench point-read shape (SELECT a column, not `*`; predicate on a
/// non-PK indexed column) — the exact shape A2 targets — must return the right
/// row for every literal, through the columns-returning wire entry point.
#[test]
fn pgbench_point_read_correct_across_literals() {
    let db = seed();
    for aid in 1..=50 {
        let (rows, cols) = db
            .query_with_columns(&format!("SELECT abalance FROM t50 WHERE aid = {aid}"))
            .unwrap();
        assert_eq!(cols, vec!["abalance".to_string()]);
        assert_eq!(rows.len(), 1, "aid={aid}");
        assert_eq!(rows[0].values[0], Value::Int4(((aid * 7) % 100000) as i32), "aid={aid}");
    }
}

#[test]
fn multi_predicate_and_string_literals_correct() {
    let db = seed();
    // String predicate with different literals.
    for i in [1, 25, 50] {
        let (rows, _c) = db
            .query_with_columns(&format!("SELECT aid FROM t50 WHERE name = 'n{i}'"))
            .unwrap();
        assert_eq!(rows.len(), 1, "name n{i}");
        assert_eq!(rows[0].values[0], Value::Int4(i));
    }
    // Multi-condition predicate.
    let (rows, _c) = db
        .query_with_columns("SELECT aid FROM t50 WHERE aid > 10 AND aid < 15")
        .unwrap();
    let mut got: Vec<i32> = rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int4(n) => n,
            _ => panic!(),
        })
        .collect();
    got.sort();
    assert_eq!(got, vec![11, 12, 13, 14]);
}

#[test]
fn no_match_returns_empty() {
    let db = seed();
    let (rows, _c) = db
        .query_with_columns("SELECT abalance FROM t50 WHERE aid = 99999")
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn bailing_shapes_still_correct() {
    // Queries the normalizer bails on must still return correct results via the
    // raw path.
    let db = seed();
    let (rows, _c) = db
        .query_with_columns("SELECT aid FROM t50 WHERE aid IN (1, 2, 3) ORDER BY aid")
        .unwrap();
    let got: Vec<i32> = rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int4(n) => n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![1, 2, 3]);
}
