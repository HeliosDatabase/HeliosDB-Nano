//! R0.2 write-write conflict detection tests.
//!
//! First-committer-wins for snapshot-isolation transactions; PostgreSQL
//! parity (no abort) for READ COMMITTED; transactional staging for the
//! parameterized INSERT plan arm.

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Value};

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 100)").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (2, 200)").unwrap();
    db
}

#[test]
fn lost_update_aborts_under_repeatable_read() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    let b = db.create_session("b", IsolationLevel::RepeatableRead).unwrap();

    db.begin_transaction_for_session(a).unwrap();
    db.begin_transaction_for_session(b).unwrap();

    // Both read v=100 at their snapshots; both write a derived value.
    db.execute_in_session(a, "UPDATE t SET v = 101 WHERE id = 1").unwrap();
    db.commit_transaction_for_session(a).unwrap();

    db.execute_in_session(b, "UPDATE t SET v = 150 WHERE id = 1").unwrap();
    let err = db.commit_transaction_for_session(b).unwrap_err();
    assert!(
        err.to_string().contains("serialization failure"),
        "expected serialization failure, got: {err}"
    );

    // First committer's value survives; loser applied nothing.
    let rows = db.query("SELECT * FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(rows[0].values[1], Value::Int4(101));

    // Loser can retry on a fresh snapshot and succeed.
    db.begin_transaction_for_session(b).unwrap();
    db.execute_in_session(b, "UPDATE t SET v = 150 WHERE id = 1").unwrap();
    db.commit_transaction_for_session(b).unwrap();
    let rows = db.query("SELECT * FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(rows[0].values[1], Value::Int4(150));

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

#[test]
fn read_committed_keeps_postgres_blind_write_semantics() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::ReadCommitted).unwrap();
    let b = db.create_session("b", IsolationLevel::ReadCommitted).unwrap();

    db.begin_transaction_for_session(a).unwrap();
    db.begin_transaction_for_session(b).unwrap();
    db.execute_in_session(a, "UPDATE t SET v = 101 WHERE id = 1").unwrap();
    db.commit_transaction_for_session(a).unwrap();
    db.execute_in_session(b, "UPDATE t SET v = 150 WHERE id = 1").unwrap();
    // PostgreSQL READ COMMITTED does not raise serialization failures here.
    db.commit_transaction_for_session(b).unwrap();
    let rows = db.query("SELECT * FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(rows[0].values[1], Value::Int4(150));

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

#[test]
fn embedded_global_transaction_validates() {
    let db = setup();

    // Global txn snapshots, then a session transaction commits a conflicting
    // write; the global commit must abort instead of losing the update.
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE t SET v = 111 WHERE id = 1").unwrap();

    let s = db.create_session("s", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(s).unwrap();
    db.execute_in_session(s, "UPDATE t SET v = 222 WHERE id = 1").unwrap();
    db.commit_transaction_for_session(s).unwrap();
    db.destroy_session(s).unwrap();

    let err = db.execute("COMMIT").unwrap_err();
    assert!(
        err.to_string().contains("serialization failure"),
        "expected serialization failure, got: {err}"
    );
    let rows = db.query("SELECT * FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(rows[0].values[1], Value::Int4(222), "session commit must survive");
}

#[test]
fn disjoint_keys_never_conflict() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    let b = db.create_session("b", IsolationLevel::RepeatableRead).unwrap();

    for round in 0..50 {
        db.begin_transaction_for_session(a).unwrap();
        db.begin_transaction_for_session(b).unwrap();
        db.execute_in_session(a, &format!("UPDATE t SET v = {round} WHERE id = 1")).unwrap();
        db.execute_in_session(b, &format!("UPDATE t SET v = {round} WHERE id = 2")).unwrap();
        db.commit_transaction_for_session(a).unwrap();
        db.commit_transaction_for_session(b).unwrap();
    }
    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

#[test]
fn delete_conflicts_are_detected() {
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    let b = db.create_session("b", IsolationLevel::RepeatableRead).unwrap();

    db.begin_transaction_for_session(a).unwrap();
    db.begin_transaction_for_session(b).unwrap();
    db.execute_in_session(a, "UPDATE t SET v = 1 WHERE id = 1").unwrap();
    db.commit_transaction_for_session(a).unwrap();
    db.execute_in_session(b, "DELETE FROM t WHERE id = 1").unwrap();
    let err = db.commit_transaction_for_session(b).unwrap_err();
    assert!(err.to_string().contains("serialization failure"));

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

#[test]
fn params_insert_plan_arm_rolls_back_in_session_txn() {
    // INSERT with an expression value bypasses the fast param-insert path
    // and lands in the execute_plan_with_params Insert arm — which before
    // R0.2 wrote directly to storage and survived ROLLBACK.
    let db = setup();
    let s = db.create_wire_session("w").unwrap();
    db.execute_params_for_session(s, "BEGIN", &[]).unwrap();
    db.execute_params_for_session(
        s,
        "INSERT INTO t (id, v) VALUES ($1, $2 + 1)",
        &[Value::Int4(50), Value::Int4(500)],
    )
    .unwrap();
    // Read-your-writes inside the txn.
    let rows = db
        .query_params_for_session(s, "SELECT * FROM t WHERE id = $1", &[Value::Int4(50)])
        .unwrap();
    assert_eq!(rows.len(), 1, "staged insert must be visible in-txn");
    db.execute_params_for_session(s, "ROLLBACK", &[]).unwrap();
    let rows = db.query("SELECT * FROM t WHERE id = 50", &[]).unwrap();
    assert_eq!(rows.len(), 0, "plan-arm INSERT must roll back");
    db.destroy_session(s).unwrap();
}

#[test]
fn params_insert_plan_arm_rolls_back_in_global_txn() {
    let db = setup();
    db.execute("BEGIN").unwrap();
    db.execute_params(
        "INSERT INTO t (id, v) VALUES ($1, $2 + 1)",
        &[Value::Int4(60), Value::Int4(600)],
    )
    .unwrap();
    db.execute("ROLLBACK").unwrap();
    let rows = db.query("SELECT * FROM t WHERE id = 60", &[]).unwrap();
    assert_eq!(rows.len(), 0, "global plan-arm INSERT must roll back");
}

#[test]
fn wire_serialization_failure_maps_to_40001_text() {
    // The handler maps Error::Transaction("serialization failure: ...") to
    // SQLSTATE 40001; assert the message marker the mapping keys on.
    let db = setup();
    let a = db.create_session("a", IsolationLevel::RepeatableRead).unwrap();
    let b = db.create_session("b", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();
    db.begin_transaction_for_session(b).unwrap();
    db.execute_in_session(a, "UPDATE t SET v = 1 WHERE id = 1").unwrap();
    db.commit_transaction_for_session(a).unwrap();
    db.execute_in_session(b, "UPDATE t SET v = 2 WHERE id = 1").unwrap();
    let err = db.commit_transaction_for_session(b).unwrap_err();
    assert!(err.to_string().contains("serialization failure"));
    assert!(err.to_string().contains("retry"));
    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}
