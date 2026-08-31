//! A2 integration: `query_with_columns` (the wire read path) returns correct
//! results when literal normalization is active (the default), across many
//! literal variants of the same query shape. The unit-level differential oracle
//! (src/sql/normalize.rs `differential`) proves raw==normalized directly; this
//! confirms the wired path end-to-end.

use heliosdb_nano::session::IsolationLevel;
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

/// Task #87 (the second, previously unfiled instance): the plan-normalization
/// fast path used to bail whenever the PROCESS-WIDE `any_session_txns()` counter
/// was non-zero, so one unrelated idle session sitting in a transaction silently
/// disabled normalization for every OTHER session's reads — a slowdown with
/// nothing in the logs to explain it.
///
/// The gate is now the caller's own transaction state only. This test pins the
/// correctness half of that change: with another session parked in an open
/// transaction holding uncommitted writes, an autocommit read must return
/// exactly what it returned with no such session — never that session's
/// uncommitted rows.
#[test]
fn normalized_reads_are_unaffected_by_another_sessions_open_transaction() {
    let db = seed();

    // Baseline: the answers with no session transaction open anywhere.
    let baseline: Vec<Value> = (1..=50)
        .map(|aid| {
            let (rows, _c) = db
                .query_with_columns(&format!("SELECT abalance FROM t50 WHERE aid = {aid}"))
                .unwrap();
            assert_eq!(rows.len(), 1, "aid={aid}");
            rows[0].values[0].clone()
        })
        .collect();

    // Park an unrelated session inside an open transaction with an UNCOMMITTED
    // write. Its row lives in that transaction's write-set buffer, so no other
    // session may see it — on the normalized path or the raw one.
    let parked = db.create_session("parked", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(parked).unwrap();
    db.execute_in_session(parked, "INSERT INTO t50 VALUES (777, 12345, 'uncommitted')")
        .unwrap();

    for (idx, expected) in baseline.iter().enumerate() {
        let aid = idx + 1;
        let (rows, _c) = db
            .query_with_columns(&format!("SELECT abalance FROM t50 WHERE aid = {aid}"))
            .unwrap();
        assert_eq!(rows.len(), 1, "aid={aid} row count changed");
        assert_eq!(&rows[0].values[0], expected, "aid={aid} value changed");
    }

    let (rows, _c) = db
        .query_with_columns("SELECT abalance FROM t50 WHERE aid = 777")
        .unwrap();
    assert!(
        rows.is_empty(),
        "another session's UNCOMMITTED row is visible: {rows:?}"
    );

    // ... and becomes visible only after that session commits.
    db.commit_transaction_for_session(parked).unwrap();
    let (rows, _c) = db
        .query_with_columns("SELECT abalance FROM t50 WHERE aid = 777")
        .unwrap();
    assert_eq!(rows.len(), 1, "the row must be visible once committed");
    assert_eq!(rows[0].values[0], Value::Int4(12345));

    db.destroy_session(parked).unwrap();
}
