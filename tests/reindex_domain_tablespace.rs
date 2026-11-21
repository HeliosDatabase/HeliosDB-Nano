//! `REINDEX` rebuilds, `CREATE DOMAIN` and `CREATE TABLESPACE` are refused.
//!
//! All three used to be accepted and do nothing. A script that declared a domain
//! with a CHECK got tables whose columns did not carry it; `CREATE TABLESPACE`
//! reported a tablespace that did not exist; `REINDEX` of an index that does not
//! exist reported success.
//!
//! That `REINDEX` restores entries an index lost is proved inside the engine
//! (`src/utility_ddl.rs`, `reindex_brings_back_the_entries_an_index_lost`),
//! where an index can be emptied; here: what a client sees.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

mod ddl_forms_support;
use ddl_forms_support::*;

const FIXTURE: &[&str] = &[
    "CREATE TABLE t (id INT PRIMARY KEY, v TEXT, n INT UNIQUE)",
    "CREATE INDEX t_v_idx ON t (v)",
    "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20), (3, 'a', 30)",
    "CREATE SCHEMA app",
    "CREATE TABLE app.u (id INT PRIMARY KEY, w TEXT)",
    "INSERT INTO app.u VALUES (1, 'x')",
];

#[test]
fn reindex_of_an_index_a_table_a_schema_and_the_database_succeeds_and_rows_stay_findable() {
    for family in FAMILIES {
        let db = setup(FIXTURE);
        for statement in [
            "REINDEX INDEX t_v_idx",
            "REINDEX TABLE t",
            "REINDEX (VERBOSE) TABLE t",
            "REINDEX TABLE CONCURRENTLY app.u",
            "reindex schema app;",
            "REINDEX SCHEMA public",
            "REINDEX DATABASE postgres",
            "REINDEX SYSTEM postgres",
        ] {
            ok(&db, family, statement);
            assert_eq!(
                text_rows(&db, "SELECT id FROM t WHERE id = 2"),
                vec![vec!["2"]],
                "{statement}"
            );
            assert_eq!(
                text_rows(&db, "SELECT id FROM t WHERE v = 'a' ORDER BY id"),
                vec![vec!["1"], vec!["3"]],
                "{statement}"
            );
            assert_eq!(
                text_rows(&db, "SELECT id FROM t WHERE n = 20"),
                vec![vec!["2"]],
                "{statement}"
            );
            assert_eq!(
                text_rows(&db, "SELECT w FROM app.u WHERE id = 1"),
                vec![vec!["x"]],
                "{statement}"
            );
        }
        // The rebuilt unique indexes still refuse a duplicate, and take new rows.
        let message = refused(&db, family, "INSERT INTO t VALUES (1, 'z', 99)");
        assert!(
            message.to_lowercase().contains("duplicate") || message.to_lowercase().contains("unique"),
            "{message}"
        );
        let message = refused(&db, family, "INSERT INTO t VALUES (4, 'z', 20)");
        assert!(
            message.to_lowercase().contains("duplicate") || message.to_lowercase().contains("unique"),
            "{message}"
        );
        ok(&db, family, "INSERT INTO t VALUES (4, 'z', 40)");
        assert_eq!(text_rows(&db, "SELECT id FROM t WHERE n = 40"), vec![vec!["4"]]);
    }
}

#[test]
fn reindex_of_what_does_not_exist_is_refused() {
    for family in FAMILIES {
        let db = setup(FIXTURE);
        let message = refused(&db, family, "REINDEX INDEX no_such_index");
        assert!(
            message.contains("no_such_index") && message.contains("does not exist"),
            "{message}"
        );
        let message = refused(&db, family, "REINDEX TABLE no_such_table");
        assert!(message.contains("no_such_table"), "{message}");
        refused(&db, family, "REINDEX");
        refused(&db, family, "REINDEX TABLE");
        refused(&db, family, "REINDEX SOMETHING t");
    }
}

#[test]
fn reindex_of_a_table_the_transaction_wrote_is_refused_and_the_transaction_goes_on() {
    let db = setup(FIXTURE);
    let session = db.create_wire_session("u").unwrap();
    db.execute_for_session(session, "BEGIN").unwrap();
    // Another table: allowed inside the transaction.
    db.execute_for_session(session, "REINDEX TABLE app.u").unwrap();
    assert!(db.session_in_transaction(session));
    db.execute_for_session(session, "INSERT INTO t VALUES (7, 'q', 70)")
        .unwrap();
    let error = db.execute_for_session(session, "REINDEX TABLE t").unwrap_err();
    assert!(error.to_string().contains("is not supported"), "{error}");
    let _ = db.execute_for_session(session, "ROLLBACK");
    db.destroy_session(session).unwrap();
    ok(&db, Family::Text, "REINDEX TABLE t");
    assert!(text_rows(&db, "SELECT id FROM t WHERE id = 7").is_empty());
}

#[test]
fn create_domain_and_create_tablespace_are_refused_and_say_why() {
    for family in FAMILIES {
        let db = setup(FIXTURE);
        for statement in [
            "CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0)",
            "CREATE DOMAIN us_postal_code AS TEXT NOT NULL",
            "create domain d as text;",
        ] {
            let message = refused(&db, family, statement);
            assert!(
                message.contains("CREATE DOMAIN") && message.contains("is not supported"),
                "{message}"
            );
        }
        let message = refused(&db, family, "CREATE TABLESPACE fast LOCATION '/mnt/ssd'");
        assert!(
            message.contains("CREATE TABLESPACE") && message.contains("is not supported"),
            "{message}"
        );

        // Nothing was created: a column of the "domain" has no type to take.
        refused(&db, family, "CREATE TABLE uses_domain (id INT, qty positive_int)");
        // The clean-up half of a script still runs.
        ok(&db, family, "DROP DOMAIN IF EXISTS positive_int");
        let message = refused(&db, family, "DROP DOMAIN positive_int");
        assert!(
            message.contains("positive_int") && message.contains("does not exist"),
            "{message}"
        );
    }
}

#[tokio::test]
async fn over_the_wire_the_refusals_are_0a000_and_reindex_answers() {
    let server = serve(setup(FIXTURE)).await;
    let (client, task) = connect(&server).await;
    let (code, message) = wire_refused(&client, "CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0)").await;
    assert_eq!(code, "0A000", "{message}");
    let (code, message) = wire_refused(&client, "CREATE TABLESPACE fast LOCATION '/mnt/ssd'").await;
    assert_eq!(code, "0A000", "{message}");
    wire_ok(&client, "REINDEX TABLE t").await;
    wire_ok(&client, "REINDEX INDEX t_v_idx").await;
    let (code, message) = wire_refused(&client, "REINDEX INDEX no_such_index").await;
    assert_ne!(code, "XX000", "{message}");
    assert!(code == "42704" || code == "42P01", "{code}: {message}");
    assert_eq!(
        wire_rows(&client, "SELECT id FROM t WHERE v = 'b'").await,
        vec![vec!["2"]]
    );
    task.abort();
}
