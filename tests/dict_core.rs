#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
#![allow(clippy::assert_is_empty)]
//! The foundation of the system dictionary, on the embedded route.
//!
//! Every read goes through both statement families (`query_with_columns` and
//! `query_params_with_columns`) and both must agree. The PostgreSQL wire is
//! covered by the in-module tests next to the protocol handler.

use heliosdb_nano::{Column, DataType, EmbeddedDatabase, Schema, Value};

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "NULL".to_string(),
        other => other.to_string(),
    }
}

/// Rows and column names of `sql`, required equal on both statement families.
fn both(db: &EmbeddedDatabase, sql: &str) -> (Vec<Vec<String>>, Vec<String>) {
    let (text_rows, text_cols) = db
        .query_with_columns(sql)
        .unwrap_or_else(|e| panic!("text family failed on `{sql}`: {e}"));
    let (param_rows, param_cols) = db
        .query_params_with_columns(sql, &[])
        .unwrap_or_else(|e| panic!("params family failed on `{sql}`: {e}"));
    let render = |rows: &[heliosdb_nano::Tuple]| -> Vec<Vec<String>> {
        rows.iter().map(|r| r.values.iter().map(s).collect()).collect()
    };
    let text = render(&text_rows);
    assert_eq!(text_cols, param_cols, "columns differ between families for `{sql}`");
    assert_eq!(text, render(&param_rows), "rows differ between families for `{sql}`");
    (text, text_cols)
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<String>> {
    both(db, sql).0
}

fn one(db: &EmbeddedDatabase, sql: &str) -> String {
    let r = rows(db, sql);
    assert_eq!(r.len(), 1, "`{sql}` must return one row, got {r:?}");
    r[0][0].clone()
}

/// The statement must be refused on both statement families; returns both messages.
fn refused(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    let text = db
        .execute(sql)
        .expect_err(&format!("`{sql}` must be refused (text family)"));
    let params = db
        .execute_params(sql, &[])
        .expect_err(&format!("`{sql}` must be refused (params family)"));
    vec![text.to_string(), params.to_string()]
}

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

// ---------------------------------------------------------------------------
// Item 1: producers reach the routine registry
// ---------------------------------------------------------------------------

#[test]
fn pg_proc_lists_user_functions_and_procedures() {
    let db = db();
    db.execute("CREATE TABLE dc_audit (id INTEGER)").unwrap();
    db.execute("CREATE FUNCTION dc_add(a INTEGER, b TEXT) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();
    db.execute("CREATE PROCEDURE dc_ins(p INTEGER) LANGUAGE sql AS $$INSERT INTO dc_audit VALUES ($p)$$")
        .unwrap();

    let got = rows(
        &db,
        "SELECT proname, prokind, prorettype, proargtypes, pronargs, prolang FROM pg_proc ORDER BY proname",
    );
    assert_eq!(
        got,
        vec![
            vec!["dc_add", "f", "23", "23 25", "2", "14"],
            vec!["dc_ins", "p", "2278", "23", "1", "14"],
        ],
        "pg_proc must list the function and the procedure with their types"
    );

    // The body is there, the schema joins, and the schema-qualified spelling agrees.
    assert!(one(&db, "SELECT prosrc FROM pg_proc WHERE proname = 'dc_add'").contains("SELECT $1"));
    assert_eq!(
        one(
            &db,
            "SELECT n.nspname FROM pg_catalog.pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.proname = 'dc_ins'"
        ),
        "public"
    );
    let oid_before = one(&db, "SELECT oid FROM pg_proc WHERE proname = 'dc_ins'");

    db.execute("DROP FUNCTION dc_add").unwrap();
    assert_eq!(rows(&db, "SELECT proname FROM pg_proc"), vec![vec!["dc_ins"]]);
    assert_eq!(
        one(&db, "SELECT oid FROM pg_proc WHERE proname = 'dc_ins'"),
        oid_before,
        "a routine's oid does not move when another routine is dropped"
    );
}

#[test]
fn pg_proc_is_scoped_to_its_own_database() {
    let a = db();
    let b = db();
    a.execute("CREATE FUNCTION dc_only_a(x INTEGER) RETURNS INTEGER AS $$ SELECT $1 $$ LANGUAGE sql")
        .unwrap();
    assert_eq!(rows(&a, "SELECT proname FROM pg_proc"), vec![vec!["dc_only_a"]]);
    assert!(
        rows(&b, "SELECT proname FROM pg_proc").is_empty(),
        "another open database must not see this one's routines"
    );
}
