//! `SELECT set_config('name', 'value', is_local)` sets as `SET` does and
//! answers with the value (GitHub #47).
//!
//! A dump made by `pg_dump` opens with
//! `SELECT pg_catalog.set_config('search_path', '', false);`. The function did
//! not exist ("Unknown scalar function: set_config") and a restore stopped at
//! that line, before any table.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Value};

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => panic!("not text: {other:?}"),
    }
}

fn one(db: &EmbeddedDatabase, sql: &str) -> (String, String) {
    let (rows, columns) = db
        .query_with_columns(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    assert_eq!(rows.len(), 1, "{sql}");
    (columns[0].clone(), text(&rows[0].values[0]))
}

#[test]
fn the_line_a_dump_opens_with_is_answered() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    assert_eq!(
        one(&db, "SELECT pg_catalog.set_config('search_path', '', false);"),
        ("set_config".to_string(), String::new())
    );
    // What a dump does next: every name carries its schema.
    db.execute("CREATE TABLE public.restored (id integer NOT NULL, name text)")
        .unwrap();
    db.execute("INSERT INTO public.restored VALUES (1, 'a')").unwrap();
    assert_eq!(db.query("SELECT id FROM public.restored", &[]).unwrap().len(), 1);
    assert_eq!(db.query("SELECT id FROM restored", &[]).unwrap().len(), 1);
    // Through every embedded entry point, and repeated (never a cached row).
    for _ in 0..3 {
        assert_eq!(
            db.query("SELECT pg_catalog.set_config('search_path', '', false)", &[])
                .unwrap()
                .len(),
            1
        );
        db.execute("SELECT set_config('search_path', '', false)").unwrap();
    }
}

#[test]
fn search_path_is_set_as_the_set_statement_sets_it() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SCHEMA app").unwrap();
    db.execute("CREATE TABLE app.items (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO app.items VALUES (1), (2)").unwrap();
    assert!(db.query("SELECT id FROM items", &[]).is_err());

    assert_eq!(one(&db, "SELECT set_config('search_path', 'app', false)").1, "app");
    assert_eq!(db.query("SELECT id FROM items", &[]).unwrap().len(), 2);
    assert_eq!(one(&db, "SELECT current_schema()").1, "app");
    assert_eq!(one(&db, "SHOW search_path").1, "app, public");
    assert_eq!(one(&db, "SELECT current_setting('search_path')").1, "app, public");

    // Back to the default path, with an alias for the answer.
    assert_eq!(
        one(&db, "SELECT set_config('search_path', '', false) AS path"),
        ("path".to_string(), String::new())
    );
    assert!(db.query("SELECT id FROM items", &[]).is_err());
    assert_eq!(one(&db, "SELECT current_schema()").1, "public");
}

#[test]
fn other_settings_are_set_and_read_back() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    assert_eq!(
        one(&db, "SELECT set_config('application_name', 'restore', false)").1,
        "restore"
    );
    assert_eq!(one(&db, "SELECT current_setting('application_name')").1, "restore");
    assert_eq!(
        one(&db, "SELECT set_config('statement_timeout', '30s', false)").1,
        "30s"
    );
    assert_eq!(one(&db, "SHOW statement_timeout").1, "30s");
    // A bad value is refused as SET refuses it.
    assert!(db
        .query("SELECT set_config('statement_timeout', 'soon', false)", &[])
        .is_err());
    assert_eq!(one(&db, "SHOW statement_timeout").1, "30s");
}

#[test]
fn each_connection_sets_its_own() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SCHEMA app").unwrap();
    db.execute("CREATE TABLE app.items (id INT PRIMARY KEY)").unwrap();
    let a = db.create_session("a", IsolationLevel::ReadCommitted).unwrap();
    let b = db.create_session("b", IsolationLevel::ReadCommitted).unwrap();
    let ask = |session, sql: &str| -> String {
        let (rows, _) = db
            .query_with_columns_for_session(session, sql)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        text(&rows[0].values[0])
    };
    assert_eq!(
        ask(a, "SELECT pg_catalog.set_config('search_path', 'app', false)"),
        "app"
    );
    assert_eq!(ask(a, "SELECT current_schema()"), "app");
    assert_eq!(ask(b, "SELECT current_schema()"), "public");
    assert!(db.query_with_columns_for_session(a, "SELECT id FROM items").is_ok());
    assert!(db.query_with_columns_for_session(b, "SELECT id FROM items").is_err());

    assert_eq!(
        ask(b, "SELECT set_config('application_name', 'worker', false)"),
        "worker"
    );
    assert_eq!(ask(b, "SELECT current_setting('application_name')"), "worker");
    assert_eq!(ask(a, "SELECT current_setting('application_name')"), "");
    // The bound-parameter family of the same connection.
    let rows = db
        .query_params_for_session(a, "SELECT set_config('search_path', '', false)", &[])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(ask(a, "SELECT current_schema()"), "public");
    db.destroy_session(a).unwrap();
    db.destroy_session(b).unwrap();
}

#[test]
fn a_call_that_is_not_a_statement_of_its_own_says_what_is_supported() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'app')").unwrap();
    let refused = db
        .query("SELECT set_config('search_path', name, false) FROM t", &[])
        .expect_err("not a literal");
    assert!(refused.to_string().contains("statement of its own"), "{refused}");
    // A value that is not a list of schema names never reaches a SET.
    assert!(db
        .query("SELECT set_config('search_path', 'a; DROP TABLE t', false)", &[])
        .is_err());
    assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().len(), 1);
    // A column that merely has the name is an ordinary query.
    db.execute("CREATE TABLE cfg (set_config TEXT)").unwrap();
    assert_eq!(db.query("SELECT set_config FROM cfg", &[]).unwrap().len(), 0);
}
