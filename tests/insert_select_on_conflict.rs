//! v02.7 - `INSERT ... SELECT` honours `ON CONFLICT`.
//!
//! The planner used to drop the conflict clause of an `INSERT ... SELECT`, so a
//! source row that collided with an existing key failed the whole statement with
//! a duplicate-key error. Every test runs on both executor families (text and
//! parameterised); the parameterised form binds a filter in the SELECT.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]

use heliosdb_nano::{EmbeddedDatabase, Value};

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

/// u(id PK, email) holds 1 and 2; src holds 1 and 3.
fn fixture() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, email TEXT)").unwrap();
    db.execute("CREATE TABLE src (id INT, email TEXT)").unwrap();
    db.execute("INSERT INTO u VALUES (1, 'old1'), (2, 'old2')").unwrap();
    db.execute("INSERT INTO src VALUES (1, 'new1'), (3, 'new3')").unwrap();
    db
}

/// Run `sql` (a `{F}` marks the filter) on the text family and the params family
/// of a fresh fixture, handing each resulting database to `check`.
fn both_families(sql_with_filter: &str, check: impl Fn(&EmbeddedDatabase, u64)) {
    let db = fixture();
    let n = db.execute(&sql_with_filter.replace("{F}", "0")).unwrap();
    check(&db, n);
    let db = fixture();
    let n = db.execute_params(&sql_with_filter.replace("{F}", "$1"), &[Value::Int4(0)]).unwrap();
    check(&db, n);
}

#[test]
fn do_nothing_skips_conflicting_rows_and_reports_inserted_count() {
    both_families(
        "INSERT INTO u (id, email) SELECT id, email FROM src WHERE id > {F} ON CONFLICT DO NOTHING",
        |db, n| {
            assert_eq!(n, 1);
            assert_eq!(rows(db, "SELECT * FROM u"), ["1|old1", "2|old2", "3|new3"]);
        },
    );
}

#[test]
fn do_update_with_target() {
    both_families(
        "INSERT INTO u (id, email) SELECT id, email FROM src WHERE id > {F} ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email",
        |db, n| {
            assert_eq!(n, 2);
            assert_eq!(rows(db, "SELECT * FROM u"), ["1|new1", "2|old2", "3|new3"]);
        },
    );
}

#[test]
fn do_update_targetless() {
    both_families(
        "INSERT INTO u (id, email) SELECT id, email FROM src WHERE id > {F} ON CONFLICT DO UPDATE SET email = EXCLUDED.email",
        |db, _n| {
            assert_eq!(rows(db, "SELECT * FROM u"), ["1|new1", "2|old2", "3|new3"]);
        },
    );
}

#[test]
fn do_update_with_where_only_updates_matching_rows() {
    both_families(
        "INSERT INTO u (id, email) SELECT id, email FROM src WHERE id > {F} ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email WHERE u.email = 'nope'",
        |db, _n| {
            assert_eq!(rows(db, "SELECT * FROM u"), ["1|old1", "2|old2", "3|new3"]);
        },
    );
}

#[test]
fn conflict_on_non_target_constraint_raises_23505() {
    let db = fixture();
    db.execute("CREATE TABLE v (id INT PRIMARY KEY, email TEXT UNIQUE)").unwrap();
    db.execute("INSERT INTO v VALUES (10, 'new3')").unwrap();
    // src row 3 collides on `email`, not on the named target `id`.
    let err = db
        .execute("INSERT INTO v (id, email) SELECT id, email FROM src WHERE id = 3 ON CONFLICT (id) DO NOTHING")
        .unwrap_err()
        .to_string()
        .to_lowercase();
    assert!(err.contains("unique") || err.contains("duplicate"), "{err}");
    assert_eq!(rows(&db, "SELECT * FROM v"), ["10|new3"]);
}

#[test]
fn returning_lists_inserted_and_updated_rows() {
    let db = fixture();
    let (n, ret) = db
        .execute_returning(
            "INSERT INTO u (id, email) SELECT id, email FROM src ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email RETURNING id, email",
        )
        .unwrap();
    assert_eq!(n, 2);
    let mut got: Vec<String> = ret.iter().map(|t| t.values.iter().map(text).collect::<Vec<_>>().join("|")).collect();
    got.sort();
    assert_eq!(got, ["1|new1", "3|new3"]);

    let db = fixture();
    let (_n, ret) = db
        .execute_params_returning(
            "INSERT INTO u (id, email) SELECT id, email FROM src WHERE id > $1 ON CONFLICT DO NOTHING RETURNING id",
            &[Value::Int4(0)],
        )
        .unwrap();
    assert_eq!(ret.len(), 1);
    assert_eq!(text(&ret[0].values[0]), "3");
}

#[test]
fn inside_begin_rollback_leaves_nothing() {
    let db = fixture();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO u (id, email) SELECT id, email FROM src ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email")
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM u"), ["1|new1", "2|old2", "3|new3"]);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(rows(&db, "SELECT * FROM u"), ["1|old1", "2|old2"]);
}

#[test]
fn source_reads_the_target_table() {
    let db = fixture();
    db.execute("INSERT INTO u (id, email) SELECT id, email || 'x' FROM u ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email")
        .unwrap();
    assert_eq!(rows(&db, "SELECT * FROM u"), ["1|old1x", "2|old2x"]);
}

#[test]
fn plain_insert_select_still_raises_on_a_duplicate_key() {
    let db = fixture();
    let err = db.execute("INSERT INTO u (id, email) SELECT id, email FROM src").unwrap_err().to_string();
    assert!(err.to_lowercase().contains("unique") || err.to_lowercase().contains("duplicate"), "{err}");
}
