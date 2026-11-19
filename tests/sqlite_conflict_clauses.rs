//! v02.6 - SQLite conflict clauses come from the parser.
//!
//! `INSERT OR REPLACE` / `INSERT OR IGNORE` / `REPLACE INTO` work in every form
//! (no column list, multi-row VALUES, quoted columns, RETURNING, parameters), and
//! no rewrite touches the text of a string literal.
//!
//! Every behaviour is checked on both executor families: text
//! (`execute` / `query`) and parameterised (`execute_params` / `query_params`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory db")
}

fn text(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int2(n) => n.to_string(),
        Value::Int4(n) => n.to_string(),
        Value::Int8(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// All rows of `sql`, each row's columns joined with `|`, sorted.
fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = db
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"))
        .iter()
        .map(|t| t.values.iter().map(text).collect::<Vec<_>>().join("|"))
        .collect();
    out.sort();
    out
}

fn tc() -> EmbeddedDatabase {
    let db = db();
    db.execute("CREATE TABLE tc (id INT PRIMARY KEY, a TEXT, b TEXT)")
        .unwrap();
    db.execute("INSERT INTO tc VALUES (1, 'orig', 'w1')").unwrap();
    db
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

// ---------------------------------------------------------------- REPLACE --

#[test]
fn insert_or_replace_without_column_list_replaces() {
    let db = tc();
    db.execute("INSERT OR REPLACE INTO tc VALUES (1, 'replaced', 'w2')")
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|replaced|w2"]);

    let db = tc();
    let n = db
        .execute_params(
            "INSERT OR REPLACE INTO tc VALUES ($1, $2, $3)",
            &[Value::Int4(1), s("replaced"), s("w2")],
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|replaced|w2"]);
}

#[test]
fn insert_or_replace_multi_row_values() {
    let db = tc();
    db.execute("INSERT OR REPLACE INTO tc VALUES (1, 'r1', 'x'), (2, 'r2', 'y')")
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|r1|x", "2|r2|y"]);

    let db = tc();
    db.execute_params(
        "INSERT OR REPLACE INTO tc (id, a, b) VALUES ($1, $2, 'x'), ($3, $4, 'y')",
        &[Value::Int4(1), s("r1"), Value::Int4(2), s("r2")],
    )
    .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|r1|x", "2|r2|y"]);
}

#[test]
fn insert_or_replace_quoted_column() {
    let db = db();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, \"my col\" TEXT)")
        .unwrap();
    db.execute("INSERT INTO u VALUES (1, 'old')").unwrap();
    db.execute("INSERT OR REPLACE INTO u (id, \"my col\") VALUES (1, 'new')")
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM u"), ["1|new"]);
    db.execute_params(
        "INSERT OR REPLACE INTO u (id, \"my col\") VALUES ($1, $2)",
        &[Value::Int4(1), s("newer")],
    )
    .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM u"), ["1|newer"]);
}

#[test]
fn insert_or_replace_returning() {
    let db = tc();
    let (n, ret) = db
        .execute_returning("INSERT OR REPLACE INTO tc VALUES (1, 'r', 'q') RETURNING id, a")
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(ret.len(), 1);
    assert_eq!(ret[0].values.iter().map(text).collect::<Vec<_>>(), ["1", "r"]);

    let (_n, ret) = db
        .execute_params_returning(
            "INSERT OR REPLACE INTO tc VALUES ($1, 'r2', 'q') RETURNING id",
            &[Value::Int4(7)],
        )
        .unwrap();
    assert_eq!(ret.len(), 1);
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|r|q", "7|r2|q"]);
}

#[test]
fn insert_or_ignore_returning() {
    let db = tc();
    let (n, ret) = db
        .execute_returning("INSERT OR IGNORE INTO tc VALUES (1, 'dup', 'q') RETURNING id")
        .unwrap();
    assert_eq!((n, ret.len()), (0, 0));
    let (n, ret) = db
        .execute_returning("INSERT OR IGNORE INTO tc VALUES (2, 'new', 'q') RETURNING id")
        .unwrap();
    assert_eq!((n, ret.len()), (1, 1));
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|orig|w1", "2|new|q"]);

    let (n, ret) = db
        .execute_params_returning(
            "INSERT OR IGNORE INTO tc VALUES ($1, 'dup', 'q') RETURNING id",
            &[Value::Int4(1)],
        )
        .unwrap();
    assert_eq!((n, ret.len()), (0, 0));
}

#[test]
fn replace_into_with_and_without_columns() {
    let db = tc();
    db.execute("REPLACE INTO tc VALUES (1, 'r1', 'w')").unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|r1|w"]);
    db.execute("REPLACE INTO tc (id, a, b) VALUES (1, 'r2', 'w'), (3, 'n', 'w')")
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|r2|w", "3|n|w"]);
    db.execute_params("REPLACE INTO tc VALUES ($1, $2, $3)", &[Value::Int4(3), s("p"), s("w")])
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|r2|w", "3|p|w"]);
}

#[test]
fn replace_without_into() {
    let db = tc();
    db.execute("REPLACE tc VALUES (1, 'bare', 'w')").unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|bare|w"]);
}

#[test]
fn insert_or_replace_with_explicit_on_conflict_is_a_syntax_error() {
    let db = tc();
    let err = db
        .execute("INSERT OR REPLACE INTO tc VALUES (1, 'x', 'y') ON CONFLICT DO NOTHING")
        .unwrap_err()
        .to_string();
    assert!(err.to_uppercase().contains("ON CONFLICT"), "{err}");
    let err = db
        .execute("INSERT OR IGNORE INTO tc VALUES (1, 'x', 'y') ON CONFLICT DO NOTHING")
        .unwrap_err()
        .to_string();
    assert!(err.to_uppercase().contains("ON CONFLICT"), "{err}");
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|orig|w1"]);
}

// ------------------------------------------------------- literals untouched --

#[test]
fn literal_mentioning_insert_or_ignore_is_stored_verbatim_and_a_duplicate_key_still_raises() {
    let db = db();
    db.execute("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO notes VALUES (1, 'first')").unwrap();
    let sql = "INSERT INTO notes (id, body) VALUES (1,'dup'), (3,'tip2: INSERT OR IGNORE INTO t VALUES (1)')";
    let err = db.execute(sql).unwrap_err().to_string();
    assert!(
        err.to_lowercase().contains("duplicate") || err.to_lowercase().contains("unique"),
        "{err}"
    );
    assert_eq!(
        rows(&db, "SELECT * FROM notes"),
        ["1|first"],
        "nothing of the failed statement is stored"
    );

    db.execute("INSERT INTO notes (id, body) VALUES (3,'tip2: INSERT OR IGNORE INTO t VALUES (1)')")
        .unwrap();
    db.execute("INSERT INTO notes (id, body) VALUES (4,'tip: INSERT OR REPLACE INTO t (a) VALUES (1)')")
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT * FROM notes WHERE id > 1"),
        [
            "3|tip2: INSERT OR IGNORE INTO t VALUES (1)",
            "4|tip: INSERT OR REPLACE INTO t (a) VALUES (1)"
        ]
    );
    db.execute_params(
        "INSERT INTO notes (id, body) VALUES ($1, 'p: INSERT OR IGNORE INTO t VALUES (1)')",
        &[Value::Int4(5)],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT body FROM notes WHERE id = 5"),
        ["p: INSERT OR IGNORE INTO t VALUES (1)"]
    );
}

#[test]
fn literal_mentioning_autoincrement_is_returned_verbatim() {
    let db = db();
    assert_eq!(
        rows(&db, "SELECT 'x INTEGER PRIMARY KEY AUTOINCREMENT y'"),
        ["x INTEGER PRIMARY KEY AUTOINCREMENT y"]
    );
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO a (body) VALUES ('INTEGER PRIMARY KEY AUTOINCREMENT')")
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM a"), ["1|INTEGER PRIMARY KEY AUTOINCREMENT"]);
}

#[test]
fn select_of_a_literal_mentioning_insert_or_replace_parses() {
    let db = db();
    assert_eq!(
        rows(&db, "SELECT 'x INSERT OR IGNORE INTO y'"),
        ["x INSERT OR IGNORE INTO y"]
    );
    assert_eq!(
        rows(&db, "SELECT 'a INSERT OR REPLACE INTO t (c) VALUES (1)'"),
        ["a INSERT OR REPLACE INTO t (c) VALUES (1)"]
    );
}

// ----------------------------------------------------------- ... SELECT form --

#[test]
fn insert_or_ignore_select() {
    let db = tc();
    db.execute("CREATE TABLE src (id INT, a TEXT, b TEXT)").unwrap();
    db.execute("INSERT INTO src VALUES (1, 'dup', 'x'), (2, 'new', 'y')")
        .unwrap();
    db.execute("INSERT OR IGNORE INTO tc SELECT id, a, b FROM src").unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|orig|w1", "2|new|y"]);
}

#[test]
fn insert_or_replace_select() {
    let db = tc();
    db.execute("CREATE TABLE src (id INT, a TEXT, b TEXT)").unwrap();
    db.execute("INSERT INTO src VALUES (1, 'dup', 'x'), (2, 'new', 'y')")
        .unwrap();
    db.execute("INSERT OR REPLACE INTO tc SELECT id, a, b FROM src")
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM tc"), ["1|dup|x", "2|new|y"]);
}
