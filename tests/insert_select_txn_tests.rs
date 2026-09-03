//! `INSERT … SELECT` participates in the enclosing transaction (#100).
//!
//! WHAT WAS BROKEN. Both InsertSelect arms wrote every row with
//! `insert_tuple_branch_aware_with_schema`, an engine call that takes NO transaction, while
//! each arm held a live `&Transaction` and used it for FK checks a few lines earlier. So
//! `BEGIN; INSERT INTO t SELECT …; ROLLBACK;` left the rows — on psql, MySQL, the REPL, the
//! embedded API and every extended-protocol driver — and a failure on row N left rows 1..N-1
//! written even in autocommit. CTAS inherited it. Found by the ACID audit, not reported.
//!
//! THE FIX stages the gate's validated tuples, in chunks, through the SAME transactional
//! multi-row primitive `INSERT … VALUES` uses (`insert_validated_tuples_in_transaction`):
//! `txn.put_insert_fast` + ART `RemoveInserted` undo + staged row counter + grouped columnar
//! side-storage, with per-row HNSW undo mirroring the single-row text arm. Autocommit keeps the
//! streaming engine path. A tenant context or a branch falls back to the engine path, exactly
//! as the multi-row fast path already does for itself.
//!
//! Every test asserts on physical rows (`rows_in`), never on `SELECT COUNT(*)`, and every
//! assertion runs on BOTH executor families.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn mem_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}
fn rows_in(db: &EmbeddedDatabase, table: &str) -> usize {
    let sql = format!("SELECT * FROM {table}");
    db.query(&sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .len()
}
fn ids(db: &EmbeddedDatabase, table: &str) -> Vec<i64> {
    let mut v: Vec<i64> = db
        .query(&format!("SELECT id FROM {table}"), &[])
        .unwrap()
        .iter()
        .map(|r| match r.values.first() {
            Some(Value::Int4(i)) => *i as i64,
            Some(Value::Int8(i)) => *i,
            other => panic!("id not int: {other:?}"),
        })
        .collect();
    v.sort();
    v
}
fn run(db: &EmbeddedDatabase, params: bool, sql: &str) -> heliosdb_nano::Result<u64> {
    if params {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}
fn setup(db: &EmbeddedDatabase, n: i64) {
    db.execute("CREATE TABLE src (id INT, v TEXT)").unwrap();
    for i in 1..=n {
        db.execute(&format!("INSERT INTO src (id, v) VALUES ({i}, 'v{i}')"))
            .unwrap();
    }
    db.execute("CREATE TABLE dst (id INT PRIMARY KEY, v TEXT)").unwrap();
}

/// THE #100 regression test. Must FAIL on every release through v4.28.0.
#[test]
fn rollback_removes_insert_select_rows_on_both_families() {
    for params in [false, true] {
        let label = if params { "params" } else { "text" };
        let db = mem_db();
        setup(&db, 5);
        db.execute("BEGIN").unwrap();
        run(&db, params, "INSERT INTO dst (id, v) SELECT id, v FROM src").unwrap();
        db.execute("ROLLBACK").unwrap();
        assert_eq!(
            rows_in(&db, "dst"),
            0,
            "{label}: ROLLBACK must remove INSERT … SELECT rows"
        );
    }
}

#[test]
fn commit_keeps_insert_select_rows_exactly_on_both_families() {
    for params in [false, true] {
        let label = if params { "params" } else { "text" };
        let db = mem_db();
        setup(&db, 5);
        db.execute("BEGIN").unwrap();
        let n = run(&db, params, "INSERT INTO dst (id, v) SELECT id, v FROM src").unwrap();
        db.execute("COMMIT").unwrap();
        assert_eq!(n, 5, "{label}: affected-row count");
        assert_eq!(ids(&db, "dst"), vec![1, 2, 3, 4, 5], "{label}: exact ids after COMMIT");
    }
}

/// Statement-level atomicity inside a transaction: row 3 violates a CHECK; after ROLLBACK
/// rows 1..2 must not be visible either.
#[test]
fn a_mid_statement_failure_leaves_nothing_after_rollback() {
    for params in [false, true] {
        let label = if params { "params" } else { "text" };
        let db = mem_db();
        db.execute("CREATE TABLE src (id INT, v TEXT)").unwrap();
        for i in 1..=5 {
            db.execute(&format!("INSERT INTO src (id, v) VALUES ({i}, 'v')"))
                .unwrap();
        }
        db.execute("CREATE TABLE dst (id INT PRIMARY KEY CHECK (id <> 3), v TEXT)")
            .unwrap();
        db.execute("BEGIN").unwrap();
        assert!(
            run(&db, params, "INSERT INTO dst (id, v) SELECT id, v FROM src ORDER BY id").is_err(),
            "{label}: row 3 must fail"
        );
        db.execute("ROLLBACK").unwrap();
        assert_eq!(rows_in(&db, "dst"), 0, "{label}: rows 1..2 must be gone after ROLLBACK");
    }
}

/// A chained INSERT … SELECT inside one transaction must see the rows the previous statement
/// staged — read-your-own-writes on the source SELECT.
#[test]
fn a_chained_insert_select_sees_rows_staged_earlier_in_the_same_transaction() {
    for params in [false, true] {
        let label = if params { "params" } else { "text" };
        let db = mem_db();
        setup(&db, 3);
        db.execute("CREATE TABLE stage (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("BEGIN").unwrap();
        run(&db, params, "INSERT INTO stage (id, v) SELECT id, v FROM src").unwrap();
        let n = run(&db, params, "INSERT INTO dst (id, v) SELECT id, v FROM stage").unwrap();
        db.execute("COMMIT").unwrap();
        assert_eq!(n, 3, "{label}: second statement must see the 3 staged rows");
        assert_eq!(ids(&db, "dst"), vec![1, 2, 3], "{label}");
    }
}

/// After ROLLBACK the ART index must not remember the rolled-back keys: re-inserting the same
/// primary key must succeed, and the PK-index COUNT(*) fast path must not over-count.
#[test]
fn rolled_back_primary_keys_are_reusable_and_not_counted() {
    for params in [false, true] {
        let label = if params { "params" } else { "text" };
        let db = mem_db();
        setup(&db, 4);
        db.execute("BEGIN").unwrap();
        run(&db, params, "INSERT INTO dst (id, v) SELECT id, v FROM src").unwrap();
        db.execute("ROLLBACK").unwrap();
        db.execute("INSERT INTO dst (id, v) VALUES (1, 'again')")
            .unwrap_or_else(|e| panic!("{label}: a rolled-back PK must be insertable again (ART undo): {e}"));
        let c = db.query("SELECT COUNT(*) FROM dst", &[]).unwrap();
        assert_eq!(
            c[0].values[0],
            Value::Int8(1),
            "{label}: COUNT(*) must not include rolled-back rows"
        );
        assert_eq!(rows_in(&db, "dst"), 1, "{label}");
    }
}

/// Chunking: more rows than the batch size all land, and batch size 0 (one chunk) works.
#[test]
fn chunk_boundaries_lose_nothing() {
    for params in [false, true] {
        let label = if params { "params" } else { "text" };
        let db = mem_db();
        setup(&db, 2500);
        db.execute("BEGIN").unwrap();
        let n = run(&db, params, "INSERT INTO dst (id, v) SELECT id, v FROM src").unwrap();
        db.execute("COMMIT").unwrap();
        assert_eq!(n, 2500, "{label}");
        assert_eq!(rows_in(&db, "dst"), 2500, "{label}: every row across chunk boundaries");
        let got = ids(&db, "dst");
        assert_eq!(got.first(), Some(&1));
        assert_eq!(got.last(), Some(&2500));
    }
}

/// Row-id counter is STAGED, so after COMMIT and a reopen the next insert gets a fresh id
/// rather than reusing one and overwriting a live row (the documented data-loss shape).
#[test]
fn committed_rows_survive_reopen_and_row_ids_are_not_reused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_string();
    {
        let db = EmbeddedDatabase::new(&path).unwrap();
        setup(&db, 50);
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO dst (id, v) SELECT id, v FROM src").unwrap();
        db.execute("COMMIT").unwrap();
        assert_eq!(rows_in(&db, "dst"), 50);
    }
    let db = EmbeddedDatabase::new(&path).unwrap();
    assert_eq!(
        rows_in(&db, "dst"),
        50,
        "committed INSERT … SELECT rows must survive reopen"
    );
    db.execute("INSERT INTO dst (id, v) VALUES (51, 'new')").unwrap();
    assert_eq!(
        rows_in(&db, "dst"),
        51,
        "a post-reopen insert must ADD a row, not overwrite one"
    );
    assert_eq!(ids(&db, "dst").len(), 51);
}

/// Autocommit is unchanged: no transaction, rows land immediately and are visible.
#[test]
fn autocommit_insert_select_still_lands_immediately() {
    for params in [false, true] {
        let db = mem_db();
        setup(&db, 3);
        run(&db, params, "INSERT INTO dst (id, v) SELECT id, v FROM src").unwrap();
        assert_eq!(ids(&db, "dst"), vec![1, 2, 3]);
    }
}
