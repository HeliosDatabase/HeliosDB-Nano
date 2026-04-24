//! Phase-2 regression: `lsp_*` functions callable from SQL via the
//! pre-parser rewriter in `src/code_graph/sql_rewrite.rs`.

#![cfg(feature = "code-graph")]

use heliosdb_nano::{
    code_graph::CodeIndexOptions, EmbeddedDatabase, Result, Value,
};

fn setup() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    Ok(db)
}

fn insert(db: &EmbeddedDatabase, path: &str, lang: &str, content: &str) -> Result<()> {
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, $2, $3)",
        &[
            Value::String(path.into()),
            Value::String(lang.into()),
            Value::String(content.into()),
        ],
    )?;
    Ok(())
}

#[test]
fn lsp_definition_via_sql() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "m.rs",
        "rust",
        "pub fn answer() -> i32 { 42 }\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;

    let rows = db.query("SELECT * FROM lsp_definition('answer')", &[])?;
    assert_eq!(rows.len(), 1);
    // Columns: symbol_id, path, line, signature, qualified, kind.
    match &rows[0].values[1] {
        Value::String(s) => assert_eq!(s, "m.rs"),
        v => panic!("expected path string, got {v:?}"),
    }
    Ok(())
}

#[test]
fn lsp_definition_with_path_hint_via_sql() -> Result<()> {
    let db = setup()?;
    insert(&db, "a.rs", "rust", "pub fn shared() {}\n")?;
    insert(&db, "b.rs", "rust", "pub fn shared() {}\n")?;
    db.code_index(CodeIndexOptions::for_table("src"))?;

    let rows = db.query(
        "SELECT * FROM lsp_definition('shared', 'b.rs')",
        &[],
    )?;
    assert_eq!(rows.len(), 1);
    match &rows[0].values[1] {
        Value::String(s) => assert_eq!(s, "b.rs"),
        v => panic!("expected path, got {v:?}"),
    }
    Ok(())
}

#[test]
fn lsp_references_via_sql() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "p.py",
        "python",
        "def helper():\n    return 1\n\ndef caller():\n    return helper() + helper()\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;

    let defs = db.query("SELECT * FROM lsp_definition('helper')", &[])?;
    let helper_id = match &defs[0].values[0] {
        Value::Int4(n) => *n as i64,
        Value::Int8(n) => *n,
        v => panic!("expected id, got {v:?}"),
    };

    let refs = db.query_params(
        "SELECT * FROM lsp_references($1)",
        &[Value::Int8(helper_id)],
    )?;
    assert_eq!(refs.len(), 2);
    Ok(())
}

#[test]
fn lsp_hover_via_sql() -> Result<()> {
    let db = setup()?;
    insert(&db, "h.rs", "rust", "pub fn hoverable() {}\n")?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let defs = db.query("SELECT * FROM lsp_definition('hoverable')", &[])?;
    let id = match &defs[0].values[0] {
        Value::Int4(n) => *n as i64,
        Value::Int8(n) => *n,
        v => panic!("expected id, got {v:?}"),
    };
    let rows = db.query_params(
        "SELECT * FROM lsp_hover($1)",
        &[Value::Int8(id)],
    )?;
    assert_eq!(rows.len(), 1);
    match &rows[0].values[0] {
        Value::String(s) => assert!(s.contains("hoverable")),
        v => panic!("expected signature string, got {v:?}"),
    }
    Ok(())
}

#[test]
fn pass_through_non_lsp_query() -> Result<()> {
    let db = setup()?;
    insert(&db, "x.rs", "rust", "pub fn x() {}\n")?;
    // Plain query without lsp_* must work unchanged.
    let rows = db.query("SELECT COUNT(*) FROM src", &[])?;
    assert_eq!(rows.len(), 1);
    Ok(())
}
