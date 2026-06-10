//! R0.1 per-session transaction correctness tests.
//!
//! These cover the wire-handler contract: every connection gets an isolated
//! session transaction (no cross-connection bleed), read-your-writes inside a
//! transaction, per-session ART undo isolation, and orphaned-connection
//! cleanup.

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Value};

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db
}

#[test]
fn no_cross_session_transaction_bleed() {
    let db = setup();
    let a = db.create_wire_session("a").unwrap();
    let b = db.create_wire_session("b").unwrap();

    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1, 'a-uncommitted')")
        .unwrap();

    // B must not see A's uncommitted row…
    let (rows, _) = db.query_with_columns_for_session(b, "SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 0, "B sees A's uncommitted row: cross-session bleed");

    // …and B's autocommit INSERT must not fold into A's transaction.
    db.execute_for_session(b, "INSERT INTO t (id, v) VALUES (2, 'b-autocommit')")
        .unwrap();

    db.execute_for_session(a, "ROLLBACK").unwrap();

    let (rows, _) = db.query_with_columns_for_session(b, "SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 1, "B's autocommit row must survive A's rollback");
    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

#[test]
fn read_your_writes_in_session_transaction() {
    let db = setup();
    let a = db.create_wire_session("a").unwrap();
    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1, 'mine')").unwrap();
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM t WHERE id = 1")
        .unwrap();
    assert_eq!(rows.len(), 1, "session transaction must see its own uncommitted write");
    db.execute_for_session(a, "ROLLBACK").unwrap();
    let (rows, _) = db
        .query_with_columns_for_session(a, "SELECT * FROM t WHERE id = 1")
        .unwrap();
    assert_eq!(rows.len(), 0, "rolled-back write must be invisible");
    db.destroy_session(a).unwrap();
}

#[test]
fn rollback_cleans_art_index() {
    // Point lookups go through the ART PK index; a rolled-back session insert
    // must not leave a phantom index entry (per-session undo log).
    let db = setup();
    let a = db.create_wire_session("a").unwrap();
    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (42, 'phantom')")
        .unwrap();
    db.execute_for_session(a, "ROLLBACK").unwrap();
    db.destroy_session(a).unwrap();

    let rows = db.query("SELECT * FROM t WHERE id = 42", &[]).unwrap();
    assert_eq!(rows.len(), 0, "phantom ART entry served after session rollback");

    // Committed session rows must KEEP their index entries even after an
    // unrelated global-slot rollback (undo-log isolation between the two).
    let b = db.create_wire_session("b").unwrap();
    db.execute_for_session(b, "BEGIN").unwrap();
    db.execute_for_session(b, "INSERT INTO t (id, v) VALUES (7, 'kept')").unwrap();
    db.execute_for_session(b, "COMMIT").unwrap();
    db.destroy_session(b).unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (8, 'global')").unwrap();
    db.execute("ROLLBACK").unwrap();

    let rows = db.query("SELECT * FROM t WHERE id = 7", &[]).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "committed session row lost its index entry after an unrelated global rollback"
    );
}

#[test]
fn extended_protocol_params_in_session_transaction() {
    let db = setup();
    let a = db.create_wire_session("a").unwrap();
    db.execute_params_for_session(a, "BEGIN", &[]).unwrap();
    db.execute_params_for_session(
        a,
        "INSERT INTO t (id, v) VALUES ($1, $2)",
        &[Value::Int4(1), Value::String("p".into())],
    )
    .unwrap();
    // Read-your-writes through the extended-protocol params path.
    let rows = db
        .query_params_for_session(a, "SELECT * FROM t WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    assert_eq!(rows.len(), 1, "params path must see the txn's own write");
    db.execute_params_for_session(a, "ROLLBACK", &[]).unwrap();
    let rows = db
        .query_params_for_session(a, "SELECT * FROM t WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    assert_eq!(rows.len(), 0);
    db.destroy_session(a).unwrap();
}

#[test]
fn concurrent_session_transactions_commit_correctly() {
    let db = setup();
    std::thread::scope(|s| {
        for t in 0..16usize {
            let dbr = &db;
            s.spawn(move || {
                let sid = dbr.create_wire_session(&format!("u{t}")).unwrap();
                for i in 0..50usize {
                    let id = 1000 + t * 100 + i;
                    dbr.execute_for_session(sid, "BEGIN").unwrap();
                    dbr.execute_for_session(sid, &format!("INSERT INTO t (id, v) VALUES ({id}, 'x')"))
                        .unwrap();
                    dbr.execute_for_session(sid, "COMMIT").unwrap();
                }
                dbr.destroy_session(sid).unwrap();
            });
        }
    });
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 16 * 50, "every committed session row must be present");
}

#[test]
fn destroy_session_rolls_back_open_transaction() {
    let db = setup();
    let a = db.create_wire_session("a").unwrap();
    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1, 'orphan')").unwrap();
    db.destroy_session(a).unwrap(); // connection dropped mid-transaction
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 0, "orphaned transaction must be rolled back");
    let rows = db.query("SELECT * FROM t WHERE id = 1", &[]).unwrap();
    assert_eq!(rows.len(), 0, "no phantom index entry from the orphaned txn");
}

#[test]
fn snapshot_isolation_vs_autocommit_fast_insert() {
    // A RepeatableRead session transaction must not see autocommit fast-path
    // inserts that commit after its snapshot. This guards the TT-aware gate
    // that keeps autocommit INSERT fast paths enabled while session
    // transactions are open (time-travel versioning on by default).
    let db = setup();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'before')").unwrap();
    let a = db.create_session("rr", IsolationLevel::RepeatableRead).unwrap();
    db.begin_transaction_for_session(a).unwrap();
    let (rows, _) = db.query_with_columns_for_session(a, "SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    // Concurrent autocommit insert — takes the fast path under the new gate.
    db.execute("INSERT INTO t (id, v) VALUES (2, 'after-snapshot')").unwrap();
    let (rows, _) = db.query_with_columns_for_session(a, "SELECT * FROM t").unwrap();
    assert_eq!(
        rows.len(),
        1,
        "RepeatableRead snapshot saw a post-snapshot autocommit fast insert"
    );
    db.rollback_transaction_for_session(a).unwrap();
    db.destroy_session(a).unwrap();

    // After the transaction ends, both rows are visible.
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 2);
}
