//! Regression tests for ROLLBACK TO SAVEPOINT correctness.
//!
//! Bug (fixed): eager ART/secondary-index maintenance records its undo ops in a
//! per-transaction undo log that full ROLLBACK replays, but ROLLBACK TO
//! SAVEPOINT did not. A row INSERTed after a savepoint therefore left a ghost
//! PK-index entry that survived `ROLLBACK TO SAVEPOINT` + `COMMIT`. In the pg35
//! benchmark that ghost made the next iteration's INSERT fail with a spurious
//! duplicate-key error, which left the connection wedged inside an open
//! transaction and silently degraded every later read (point lookups dropped
//! from ~0.5us to ~650us because the in-transaction path disables the ART
//! point-lookup fast path).
//!
//! The fix snapshots the undo-log length at SAVEPOINT and replays+drops exactly
//! the ops staged after it on ROLLBACK TO SAVEPOINT.

use heliosdb_nano::{EmbeddedDatabase, Value};

fn scalar_i64(db: &EmbeddedDatabase, sql: &str) -> i64 {
    let rows = db.query(sql, &[]).unwrap();
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => *n as i64,
        other => panic!("expected an integer scalar from `{sql}`, got {other:?}"),
    }
}

/// A row INSERTed after a savepoint must be fully undone — data AND index — by
/// ROLLBACK TO SAVEPOINT, leaving no residue after COMMIT.
#[test]
fn rollback_to_savepoint_undoes_post_savepoint_insert() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, label TEXT)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO t VALUES (42, 'a')").unwrap();
    db.execute("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // Index path (WHERE id = const) and full-scan path must agree the row is gone.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE id = 42"), 0);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE id = 42 + 0"), 0);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 0);
    assert!(!db.in_transaction());

    // And the PK slot is reusable — no ghost index entry causing a false dup.
    db.execute("INSERT INTO t VALUES (42, 'real')").unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE id = 42"), 1);
}

/// Work committed BEFORE the savepoint is kept; only post-savepoint work is
/// rolled back.
#[test]
fn rollback_to_savepoint_keeps_pre_savepoint_work() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, label TEXT)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'keep')").unwrap();
    db.execute("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'drop')").unwrap();
    db.execute("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE id = 1"), 1);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE id = 2"), 0);
}

/// UPDATE and DELETE undo ops (RestoreUpdated / RestoreDeleted) must also be
/// reverted on ROLLBACK TO SAVEPOINT so the secondary index matches the data.
#[test]
fn rollback_to_savepoint_undoes_post_savepoint_update_and_delete() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, age INT)").unwrap();
    db.execute("CREATE INDEX idx_age ON t(age)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("SAVEPOINT sp1").unwrap();
    db.execute("UPDATE t SET age = 99 WHERE id = 1").unwrap();
    db.execute("DELETE FROM t WHERE id = 2").unwrap();
    db.execute("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // Original values restored, reachable via the secondary index.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE age = 10"), 1);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE age = 20"), 1);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE age = 99"), 0);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 2);
}

/// Nested savepoints: rolling back to the outer one must undo work staged under
/// the inner one too.
#[test]
fn rollback_to_outer_savepoint_undoes_inner() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("SAVEPOINT outer").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("SAVEPOINT inner").unwrap();
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    db.execute("ROLLBACK TO SAVEPOINT outer").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 0);
}

/// The exact pg35 "Transaction ctl" pattern, repeated: it must neither leave a
/// duplicate-key ghost nor wedge the connection in an open transaction.
#[test]
fn repeated_savepoint_insert_rollback_commit_does_not_wedge() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE bench_ins (id INT PRIMARY KEY, label TEXT)")
        .unwrap();

    for i in 0..50 {
        db.execute("BEGIN").unwrap();
        db.execute("SAVEPOINT sp1").unwrap();
        db.execute("INSERT INTO bench_ins VALUES (99999, 'txn')")
            .unwrap_or_else(|e| panic!("iteration {i}: INSERT hit spurious error: {e}"));
        db.execute("ROLLBACK TO SAVEPOINT sp1").unwrap();
        db.execute("COMMIT").unwrap();
        assert!(
            !db.in_transaction(),
            "iteration {i}: connection left wedged in a transaction"
        );
    }

    // 99999 was rolled back every time, so the table is empty.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM bench_ins WHERE id = 99999"), 0);
}

/// RELEASE SAVEPOINT keeps the work (it merges into the enclosing transaction).
#[test]
fn release_savepoint_keeps_work() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO t VALUES (7)").unwrap();
    db.execute("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE id = 7"), 1);
}
