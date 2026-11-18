//! `COMMENT ON` is accepted (GitHub #66).
//!
//! Dumps and migration tools write `COMMENT ON TABLE … IS '…'` beside the DDL
//! it describes; the refusal ("Comment is not supported") stopped the script.
//! The comment is not stored: nothing reads one back yet. The target of a
//! table or column comment must exist, as in PostgreSQL.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::EmbeddedDatabase;

fn database() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE SCHEMA mediation").unwrap();
    db.execute("CREATE TABLE mediation.md_court (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("CREATE TABLE plain (id INT PRIMARY KEY, \"Mixed\" TEXT)")
        .unwrap();
    db.execute("INSERT INTO plain VALUES (1, 'x')").unwrap();
    db
}

/// Both statement families must agree.
fn both(db: &EmbeddedDatabase, sql: &str) -> Result<(), String> {
    let text = db.execute(sql).map(|_| ()).map_err(|error| error.to_string());
    let bound = db
        .execute_params(sql, &[])
        .map(|_| ())
        .map_err(|error| error.to_string());
    assert_eq!(text.is_ok(), bound.is_ok(), "{sql}: {text:?} against {bound:?}");
    text
}

#[test]
fn a_comment_on_a_table_or_a_column_is_accepted_and_may_be_repeated() {
    let db = database();
    for sql in [
        "COMMENT ON TABLE mediation.md_court IS 'test comment'",
        "COMMENT ON TABLE mediation.md_court IS 'replaced'",
        "COMMENT ON TABLE mediation.md_court IS NULL",
        "comment on table plain is 'lower case';",
        "COMMENT ON COLUMN plain.id IS 'the key'",
        "COMMENT ON COLUMN mediation.md_court.name IS 'the court''s name'",
        "COMMENT ON COLUMN plain.\"Mixed\" IS 'quoted'",
    ] {
        both(&db, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    // Nothing else changed.
    assert_eq!(db.query("SELECT id FROM plain", &[]).unwrap().len(), 1);
    assert_eq!(db.query("SELECT id FROM mediation.md_court", &[]).unwrap().len(), 0);
}

#[test]
fn the_forms_a_dump_writes_for_other_objects_are_accepted() {
    let db = database();
    db.execute("CREATE INDEX plain_mixed ON plain (\"Mixed\")").unwrap();
    for sql in [
        "COMMENT ON SCHEMA mediation IS 'cases'",
        "COMMENT ON INDEX plain_mixed IS 'lookup'",
        "COMMENT ON EXTENSION plpgsql IS 'PL/pgSQL procedural language'",
        "COMMENT ON CONSTRAINT plain_pkey ON plain IS 'key'",
        "COMMENT ON VIEW some_view IS 'v'",
        "COMMENT ON FUNCTION f(integer) IS 'f'",
        "COMMENT ON SEQUENCE plain_id_seq IS 's'",
    ] {
        both(&db, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
}

#[test]
fn a_comment_on_a_missing_table_or_column_is_refused() {
    let db = database();
    let missing = both(&db, "COMMENT ON TABLE nosuch IS 'x'").expect_err("no such table");
    assert!(missing.contains("does not exist"), "{missing}");
    let missing = both(&db, "COMMENT ON COLUMN plain.nosuch IS 'x'").expect_err("no such column");
    assert!(missing.contains("nosuch"), "{missing}");
    let missing = both(&db, "COMMENT ON COLUMN nosuch.id IS 'x'").expect_err("no such table");
    assert!(missing.contains("does not exist"), "{missing}");
    // An unquoted name folds to lower case; the column is "Mixed".
    assert!(both(&db, "COMMENT ON COLUMN plain.Mixed IS 'x'").is_err());
    both(&db, "COMMENT ON TABLE IF EXISTS nosuch IS 'x'").expect("IF EXISTS");
}

#[test]
fn only_a_comment_statement_is_taken() {
    let db = database();
    // A table whose name starts like the keyword is an ordinary statement.
    db.execute("CREATE TABLE comments (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO comments VALUES (1, 'COMMENT ON TABLE plain IS ''x''')")
        .unwrap();
    assert_eq!(db.query("SELECT body FROM comments", &[]).unwrap().len(), 1);
    assert!(db.execute("COMMENT").is_err());
    assert!(db.execute("COMMENTON TABLE plain IS 'x'").is_err());
}
