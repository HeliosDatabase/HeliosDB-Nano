//! Utility statements inside a session transaction (SPEC F, part F3).
//!
//! `VACUUM`, `REINDEX`, `CREATE`/`DROP DOMAIN`, `CREATE TABLESPACE`,
//! `RESET name|ALL` and `ALTER TABLE … ATTACH/DETACH PARTITION` have no
//! sqlparser grammar at all — the engine intercepts every one of them
//! BEFORE parsing, on `execute()`'s pre-parse chain. The in-transaction
//! branch of `execute_for_session` went straight to the sqlparser-first
//! planner, so all of them failed inside ANY session transaction: an
//! explicit `BEGIN … COMMIT`, and — once the PG handler wraps a
//! multi-statement simple query in an implicit block — the far more common
//! `INSERT …; VACUUM` shape.
//!
//! Both paths now share one funnel (`try_handle_utility_statement`, and its
//! result-set twin `try_handle_utility_query`), so they cannot drift again.
//! These tests pin three things per statement: it SUCCEEDS inside the
//! transaction, it leaves the transaction OPEN (it neither commits nor
//! closes it — a following write is still rolled back by `ROLLBACK`), and an
//! explicit `COMMIT` afterwards still persists the transaction's writes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::EmbeddedDatabase;

/// Every pre-parse utility statement the funnel claims. The ATTACH/DETACH
/// shapes are the ones pinned by `src/protocol/postgres/wire_tests.rs`.
const UTILITY_STATEMENTS: &[&str] = &[
    "VACUUM",
    "VACUUM ANALYZE",
    "VACUUM VERSIONS",
    "REINDEX TABLE t",
    "CREATE DOMAIN d AS TEXT",
    "DROP DOMAIN d",
    "CREATE TABLESPACE ts LOCATION '/tmp/x'",
    "RESET ALL",
    "RESET search_path",
    "ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (0) TO (100)",
    "ALTER TABLE p DETACH PARTITION c",
];

fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db.execute("CREATE TABLE p (id INT, label TEXT) PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE c PARTITION OF p FOR VALUES FROM (0) TO (100)")
        .unwrap();
    db
}

/// Inside `BEGIN`: the statement succeeds, the transaction is still open
/// afterwards, and `ROLLBACK` still takes back BOTH the write staged before
/// it and the write staged after it (so the utility statement neither
/// committed nor detached the transaction).
#[test]
fn utility_statement_inside_transaction_succeeds_and_keeps_the_transaction_open() {
    for stmt in UTILITY_STATEMENTS {
        let db = setup();
        let s = db.create_wire_session("u").unwrap();

        db.execute_for_session(s, "BEGIN").unwrap();
        db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (1, 'before')")
            .unwrap();

        db.execute_for_session(s, stmt)
            .unwrap_or_else(|e| panic!("`{stmt}` inside a session transaction must succeed, got: {e}"));

        assert!(
            db.session_in_transaction(s),
            "`{stmt}` must not close the session transaction"
        );

        // Read-your-writes still resolves through the SAME transaction.
        let (rows, _) = db.query_with_columns_for_session(s, "SELECT id FROM t").unwrap();
        assert_eq!(
            rows.len(),
            1,
            "`{stmt}`: the transaction's staged write must still be visible to it"
        );

        // A write staged AFTER the utility statement is still part of the
        // same transaction.
        db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (2, 'after')")
            .unwrap();
        db.execute_for_session(s, "ROLLBACK").unwrap();
        assert!(
            !db.session_in_transaction(s),
            "`{stmt}`: ROLLBACK must end the transaction"
        );

        let rows = db.query("SELECT id FROM t", &[]).unwrap();
        assert!(
            rows.is_empty(),
            "`{stmt}`: ROLLBACK must undo both writes — the utility statement must not have committed them, got {}",
            rows.len()
        );

        db.destroy_session(s).unwrap();
    }
}

/// The same statements must not break an explicit `COMMIT` either.
#[test]
fn utility_statement_inside_transaction_then_commit_persists_the_writes() {
    for stmt in UTILITY_STATEMENTS {
        let db = setup();
        let s = db.create_wire_session("u").unwrap();

        db.execute_for_session(s, "BEGIN").unwrap();
        db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (1, 'kept')")
            .unwrap();
        db.execute_for_session(s, stmt)
            .unwrap_or_else(|e| panic!("`{stmt}` inside a session transaction must succeed, got: {e}"));
        db.execute_for_session(s, "COMMIT").unwrap();

        assert!(
            !db.session_in_transaction(s),
            "`{stmt}`: COMMIT must end the transaction"
        );
        let rows = db.query("SELECT id FROM t", &[]).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "`{stmt}`: the committed write must survive, got {} row(s)",
            rows.len()
        );

        db.destroy_session(s).unwrap();
    }
}

/// Autocommit (no session transaction) is unchanged by the funnel refactor —
/// both through a wire session and through the bare embedded entry point.
#[test]
fn utility_statement_outside_a_transaction_still_succeeds() {
    let db = setup();
    let s = db.create_wire_session("u").unwrap();
    for stmt in UTILITY_STATEMENTS {
        db.execute_for_session(s, stmt)
            .unwrap_or_else(|e| panic!("`{stmt}` in autocommit must succeed, got: {e}"));
        assert!(
            !db.session_in_transaction(s),
            "`{stmt}` must not open a transaction on its own"
        );
        db.execute(stmt)
            .unwrap_or_else(|e| panic!("`{stmt}` through execute() must succeed, got: {e}"));
    }
    db.destroy_session(s).unwrap();
}

/// The result-set twin of the funnel: a utility statement routed through the
/// query surface inside an open transaction is handled there too.
#[test]
fn utility_query_inside_transaction_is_handled_on_the_query_path() {
    let db = setup();
    let s = db.create_wire_session("u").unwrap();
    db.execute_for_session(s, "BEGIN").unwrap();
    db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (1, 'before')")
        .unwrap();

    // Command-tag-only utility statements return no rows…
    for stmt in ["VACUUM", "REINDEX TABLE t", "CREATE TABLESPACE ts2 LOCATION '/tmp/x'"] {
        let (rows, _) = db
            .query_with_columns_for_session(s, stmt)
            .unwrap_or_else(|e| panic!("`{stmt}` on the query path inside a transaction must succeed, got: {e}"));
        assert!(rows.is_empty(), "`{stmt}` must return no rows");
        assert!(
            db.session_in_transaction(s),
            "`{stmt}` must not close the session transaction"
        );
    }

    // …and `VACUUM VERSIONS` returns its one reclaimed-count row.
    let (rows, columns) = db.query_with_columns_for_session(s, "VACUUM VERSIONS").unwrap();
    assert_eq!(rows.len(), 1, "VACUUM VERSIONS must return one row");
    assert_eq!(columns, vec!["versions_collected".to_string()]);
    assert!(db.session_in_transaction(s));

    db.execute_for_session(s, "ROLLBACK").unwrap();
    let rows = db.query("SELECT id FROM t", &[]).unwrap();
    assert!(rows.is_empty(), "ROLLBACK must undo the staged write");
    db.destroy_session(s).unwrap();
}

/// Negative control: the funnel claims utility statements ONLY — a genuinely
/// unparseable statement inside the transaction still errors rather than
/// being swallowed as a no-op.
#[test]
fn unparseable_statement_inside_transaction_still_errors() {
    let db = setup();
    let s = db.create_wire_session("u").unwrap();
    db.execute_for_session(s, "BEGIN").unwrap();
    db.execute_for_session(s, "INSERT INTO t (id, v) VALUES (1, 'before')")
        .unwrap();

    db.execute_for_session(s, "NOT A STATEMENT AT ALL")
        .expect_err("an unparseable statement inside a transaction must still error");

    // …and so does a statement that merely LOOKS like one of the no-ops.
    db.execute_for_session(s, "REINDEXED TABLE t")
        .expect_err("`REINDEXED …` must not be swallowed by the REINDEX no-op");

    let _ = db.execute_for_session(s, "ROLLBACK");
    let rows = db.query("SELECT id FROM t", &[]).unwrap();
    assert!(rows.is_empty(), "ROLLBACK must undo the staged write");
    db.destroy_session(s).unwrap();
}
