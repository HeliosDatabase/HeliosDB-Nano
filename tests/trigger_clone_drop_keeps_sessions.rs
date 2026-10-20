//! Sprinter 96e5a8cfa254: a triggered INSERT must not discard other sessions'
//! open transactions or the process's prepared statements.
//!
//! DML on a table with triggers mints a `clone_for_trigger()` handle per row
//! and drops it mid-statement. That clone shares the root's
//! `session_transactions`, `session_txn_count` and prepared-statement maps,
//! and `impl Drop for EmbeddedDatabase` used to clear all of them
//! unconditionally — outside the last-owner gate that protects the other
//! close-time actions. One triggered INSERT, from any session, therefore:
//!
//!   * dropped every other session's open transaction slot. The victim's
//!     `COMMIT` then found no slot, was a silent no-op reporting success, and
//!     the writes it had staged were gone;
//!   * deleted every `PREPARE`d statement (`EXECUTE` failed
//!     "Prepared statement '…' does not exist").
//!
//! Both tests below are RED on the unfixed tree for exactly those reasons and
//! GREEN once the clears run only for the last owner.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

/// `t` carries the one trigger form that executes: a `BEFORE INSERT … FOR
/// EACH ROW` function whose body rewrites `NEW`. `other` has no trigger and
/// is where the victim session stages its write.
fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("CREATE TABLE other (id INT PRIMARY KEY, note TEXT)")
        .unwrap();
    db.execute(
        "CREATE FUNCTION dbl_v() RETURNS TRIGGER AS $$ BEGIN NEW.v = NEW.v * 2; RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .unwrap();
    db.execute("CREATE TRIGGER t_dbl BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION dbl_v()")
        .unwrap();
    db
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// Non-vacuity: the INSERT into `t` really went through the trigger path (the
/// one that mints the clone) — the BEFORE-row rewrite doubled `v`.
fn assert_trigger_fired(db: &EmbeddedDatabase, id: i64, expected_v: i64) {
    let rows = db.query(&format!("SELECT v FROM t WHERE id = {id}"), &[]).unwrap();
    assert_eq!(rows.len(), 1, "row {id} must exist in t");
    assert_eq!(
        as_i64(&rows[0].values[0]),
        expected_v,
        "the BEFORE INSERT trigger must have rewritten NEW.v — otherwise this test exercises nothing"
    );
}

#[test]
fn triggered_insert_from_another_session_keeps_an_open_transaction() {
    let db = setup();
    let a = db.create_wire_session("a").unwrap();
    let b = db.create_wire_session("b").unwrap();

    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(a, "INSERT INTO other (id, note) VALUES (1, 'staged by a')")
        .unwrap();
    assert!(db.session_in_transaction(a), "precondition: A's transaction is open");

    // B's autocommit INSERT fires the trigger → a `clone_for_trigger()` handle
    // is created and dropped mid-statement.
    db.execute_for_session(b, "INSERT INTO t (id, v) VALUES (1, 5)")
        .unwrap();
    assert_trigger_fired(&db, 1, 10);

    // Unfixed: the clone's Drop cleared `session_transactions`, so A is no
    // longer in a transaction at all.
    assert!(
        db.session_in_transaction(a),
        "another session's triggered INSERT must not discard A's open transaction"
    );

    // A's transaction is still the one it began: its staged row is visible to
    // it (read-your-writes).
    let (own, _) = db
        .query_with_columns_for_session(a, "SELECT id FROM other WHERE id = 1")
        .unwrap();
    assert_eq!(own.len(), 1, "A must still see its own staged write");

    // Unfixed: this COMMIT was a silent no-op (no slot left) and reported
    // success while the staged row had been thrown away.
    db.execute_for_session(a, "COMMIT").expect("A's COMMIT must succeed");
    assert!(!db.session_in_transaction(a), "COMMIT must end A's transaction");
    let rows = db.query("SELECT note FROM other WHERE id = 1", &[]).unwrap();
    assert_eq!(rows.len(), 1, "A's committed row must be visible after COMMIT");
    assert_eq!(rows[0].values[0], Value::String("staged by a".to_string()));

    // The trigger row B wrote is untouched as well.
    assert_trigger_fired(&db, 1, 10);

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

#[test]
fn triggered_insert_keeps_prepared_statements() {
    let db = setup();
    db.execute("INSERT INTO other (id, note) VALUES (100, 'pre-existing')")
        .unwrap();
    // Two shapes, so both prepared-statement maps are covered: a PK point
    // lookup (eligible for the fast prepared-select spec) and a range scan
    // (stored as a full plan).
    db.query("PREPARE by_pk AS SELECT note FROM other WHERE id = $1", &[])
        .unwrap();
    db.query("PREPARE by_range AS SELECT id FROM other WHERE id >= $1", &[])
        .unwrap();
    assert_eq!(db.query("EXECUTE by_pk(100)", &[]).unwrap().len(), 1, "precondition");
    assert_eq!(db.query("EXECUTE by_range(0)", &[]).unwrap().len(), 1, "precondition");

    let b = db.create_wire_session("b").unwrap();
    db.execute_for_session(b, "INSERT INTO t (id, v) VALUES (1, 5)")
        .unwrap();
    assert_trigger_fired(&db, 1, 10);

    // A second, multi-row triggered INSERT (one clone per row) for good measure.
    db.execute("INSERT INTO t (id, v) VALUES (2, 1), (3, 2)").unwrap();
    assert_trigger_fired(&db, 2, 2);
    assert_trigger_fired(&db, 3, 4);

    // Unfixed: both fail "Prepared statement '…' does not exist".
    let pk = db
        .query("EXECUTE by_pk(100)", &[])
        .unwrap_or_else(|e| panic!("by_pk must survive a triggered INSERT, got: {e}"));
    assert_eq!(pk.len(), 1);
    assert_eq!(pk[0].values[0], Value::String("pre-existing".to_string()));
    let range = db
        .query("EXECUTE by_range(0)", &[])
        .unwrap_or_else(|e| panic!("by_range must survive a triggered INSERT, got: {e}"));
    assert_eq!(range.len(), 1);

    db.destroy_session(b).unwrap();
}
