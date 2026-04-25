//! `lsp_rename_apply` — write-back path for the rename refactor.

#![cfg(feature = "code-graph")]

use heliosdb_nano::code_graph::{CodeIndexOptions, RenameApplyOptions};
use heliosdb_nano::{EmbeddedDatabase, Value};

fn upsert(db: &EmbeddedDatabase, path: &str, body: &str) {
    db.execute_params_returning(
        "DELETE FROM src WHERE path = $1",
        &[Value::String(path.into())],
    )
    .unwrap();
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, 'rust', $2)",
        &[
            Value::String(path.into()),
            Value::String(body.into()),
        ],
    )
    .unwrap();
}

fn symbol_id_of(db: &EmbeddedDatabase, name: &str) -> i64 {
    let rows = db
        .query_params(
            "SELECT node_id FROM _hdb_code_symbols WHERE name = $1",
            &[Value::String(name.into())],
        )
        .unwrap();
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::Int4(n)) => i64::from(*n),
        Some(Value::Int8(n)) => *n,
        _ => panic!("symbol {name} not found"),
    }
}

#[test]
fn dry_run_reports_counts_without_writing() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    upsert(&db, "a.rs", "pub fn foo() {}\nfn caller() { foo(); foo(); }\n");
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    let id = symbol_id_of(&db, "foo");

    let stats = db
        .lsp_rename_apply(id, "renamed_foo", &RenameApplyOptions::dry_run())
        .unwrap();
    assert_eq!(stats.applied, false);
    assert!(stats.occurrences_replaced >= 1);

    // Source unchanged.
    let row = db
        .query_params(
            "SELECT content FROM src WHERE path = $1",
            &[Value::String("a.rs".into())],
        )
        .unwrap();
    let content = match row.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => s.clone(),
        _ => panic!("missing"),
    };
    assert!(content.contains("foo"));
    assert!(!content.contains("renamed_foo"));
}

#[test]
fn apply_writes_replacement() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    upsert(&db, "a.rs", "pub fn foo() {}\n");
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    let id = symbol_id_of(&db, "foo");

    let stats = db
        .lsp_rename_apply(id, "improved_foo", &RenameApplyOptions::apply())
        .unwrap();
    assert!(stats.applied);
    assert!(stats.files_modified >= 1);

    let row = db
        .query_params(
            "SELECT content FROM src WHERE path = $1",
            &[Value::String("a.rs".into())],
        )
        .unwrap();
    let content = match row.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => s.clone(),
        _ => panic!("missing"),
    };
    assert!(content.contains("improved_foo"), "got: {content}");
}

#[test]
fn word_boundary_prevents_substring_match() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    // `foo` and `foobar` both appear; the rename must touch only foo.
    upsert(&db, "a.rs", "pub fn foo() {}\npub fn foobar() {}\n");
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    let id = symbol_id_of(&db, "foo");

    let _ = db
        .lsp_rename_apply(id, "qux", &RenameApplyOptions::apply())
        .unwrap();
    let row = db
        .query_params(
            "SELECT content FROM src WHERE path = $1",
            &[Value::String("a.rs".into())],
        )
        .unwrap();
    let content = match row.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => s.clone(),
        _ => panic!("missing"),
    };
    assert!(content.contains("pub fn qux()"), "definition not renamed: {content}");
    assert!(content.contains("foobar"), "foobar must survive: {content}");
}

#[test]
fn empty_new_name_errors() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    upsert(&db, "a.rs", "pub fn x() {}\n");
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    let id = symbol_id_of(&db, "x");
    let r = db.lsp_rename_apply(id, "", &RenameApplyOptions::apply());
    assert!(r.is_err());
}

#[test]
fn unknown_symbol_returns_default_stats() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    upsert(&db, "a.rs", "pub fn x() {}\n");
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    let stats = db
        .lsp_rename_apply(999_999, "anything", &RenameApplyOptions::apply())
        .unwrap();
    assert_eq!(stats.files_modified, 0);
    assert_eq!(stats.occurrences_replaced, 0);
}
