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
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1, 'mine')")
        .unwrap();
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
    db.execute_for_session(b, "INSERT INTO t (id, v) VALUES (7, 'kept')")
        .unwrap();
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
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1, 'orphan')")
        .unwrap();
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
    db.execute("INSERT INTO t (id, v) VALUES (2, 'after-snapshot')")
        .unwrap();
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

// ---------------------------------------------------------------------------
// R1.3-p2: per-session SET synchronous_commit (PG-compatible)
// ---------------------------------------------------------------------------

fn durable_disk_db(tag: &str, durable: bool) -> (EmbeddedDatabase, std::path::PathBuf) {
    let tmp = std::env::temp_dir().join(format!("helios_synccommit_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut c = heliosdb_nano::Config::default();
    c.storage.path = Some(tmp.clone());
    c.storage.memory_only = false;
    c.storage.wal_enabled = true;
    c.storage.durable_commit = durable;
    let db = EmbeddedDatabase::with_config(c).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    (db, tmp)
}

#[test]
fn synchronous_commit_off_is_session_scoped() {
    let (db, tmp) = durable_disk_db("off_scoped", true);
    let a = db.create_wire_session("a").unwrap();
    let b = db.create_wire_session("b").unwrap();

    // Default: inherit storage.durable_commit = true.
    assert_eq!(db.session_synchronous_commit(a).unwrap(), None);
    assert!(db.session_synchronous_commit_effective(a).unwrap());

    db.execute_for_session(a, "SET synchronous_commit = off").unwrap();
    assert_eq!(db.session_synchronous_commit(a).unwrap(), Some(false));
    assert!(!db.session_synchronous_commit_effective(a).unwrap());
    // Session-scoped: B is untouched.
    assert_eq!(db.session_synchronous_commit(b).unwrap(), None);
    assert!(db.session_synchronous_commit_effective(b).unwrap());

    // A's transactional + autocommit writes still commit and are visible.
    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1, 'async')")
        .unwrap();
    db.execute_for_session(a, "COMMIT").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (2, 'auto')")
        .unwrap();
    let (rows, _) = db.query_with_columns_for_session(b, "SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 2, "async-commit rows must be visible immediately");

    // RESET returns to the storage default.
    db.execute_for_session(a, "RESET synchronous_commit").unwrap();
    assert_eq!(db.session_synchronous_commit(a).unwrap(), None);
    assert!(db.session_synchronous_commit_effective(a).unwrap());

    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn synchronous_commit_on_overrides_non_durable_default() {
    let (db, tmp) = durable_disk_db("on_override", false);
    let a = db.create_wire_session("a").unwrap();
    assert!(!db.session_synchronous_commit_effective(a).unwrap());
    // PG also accepts TO and the replication-oriented values.
    db.execute_for_session(a, "SET synchronous_commit TO on").unwrap();
    assert_eq!(db.session_synchronous_commit(a).unwrap(), Some(true));
    assert!(db.session_synchronous_commit_effective(a).unwrap());
    db.execute_for_session(a, "SET synchronous_commit = remote_write")
        .unwrap();
    assert_eq!(db.session_synchronous_commit(a).unwrap(), Some(true));

    // Durable-by-override commits work (group fsync on a non-durable engine).
    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1, 'sync')")
        .unwrap();
    db.execute_for_session(a, "COMMIT").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (2, 'auto-sync')")
        .unwrap();
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 2);

    db.destroy_session(a).unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn synchronous_commit_set_inside_open_transaction_applies() {
    // PG semantics: the setting may change mid-transaction and the COMMIT
    // honors the latest value (we apply it to the live transaction flag).
    let (db, tmp) = durable_disk_db("mid_txn", true);
    let a = db.create_wire_session("a").unwrap();
    db.execute_for_session(a, "BEGIN").unwrap();
    db.execute_for_session(a, "INSERT INTO t (id, v) VALUES (1, 'x')")
        .unwrap();
    db.execute_for_session(a, "SET LOCAL synchronous_commit = off").unwrap();
    db.execute_for_session(a, "COMMIT").unwrap();
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    db.destroy_session(a).unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn synchronous_commit_invalid_value_errors() {
    let db = setup();
    let a = db.create_wire_session("a").unwrap();
    let err = db
        .execute_for_session(a, "SET synchronous_commit = sideways")
        .unwrap_err();
    assert!(
        err.to_string().contains("synchronous_commit"),
        "unexpected error: {err}"
    );
    // Unrelated settings are untouched by the intercept.
    assert_eq!(db.session_synchronous_commit(a).unwrap(), None);
    db.destroy_session(a).unwrap();
}

#[test]
fn synchronous_commit_embedded_session_api() {
    // The embedded execute_in_session path honors SET synchronous_commit
    // for implicit (autocommit) session statements too.
    let (db, tmp) = durable_disk_db("embedded", true);
    let s = db.create_session("emb", IsolationLevel::ReadCommitted).unwrap();
    db.execute_in_session(s, "SET synchronous_commit = off").unwrap();
    assert_eq!(db.session_synchronous_commit(s).unwrap(), Some(false));
    db.execute_in_session(s, "INSERT INTO t (id, v) VALUES (1, 'emb')")
        .unwrap();
    db.execute_in_session(s, "RESET synchronous_commit").unwrap();
    assert_eq!(db.session_synchronous_commit(s).unwrap(), None);
    db.execute_in_session(s, "INSERT INTO t (id, v) VALUES (2, 'emb2')")
        .unwrap();
    let rows = db.query("SELECT * FROM t", &[]).unwrap();
    assert_eq!(rows.len(), 2);
    db.destroy_session(s).unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}
