//! Phase-1 MVP end-to-end tests for the code-graph track.
//!
//! Exercises the full pipeline from a source table → `code_index(...)`
//! → `lsp_*` queries. Runs against an in-memory Nano. No external
//! services; the embedder is the default `NoopEmbedder` so
//! `body_vec` stays NULL.
//!
//! Enable with `cargo test --features code-graph --test code_graph_mvp`.

#![cfg(feature = "code-graph")]

use heliosdb_nano::{
    code_graph::{CodeIndexOptions, DefinitionHint},
    EmbeddedDatabase, Result,
};

fn setup() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute(
        r#"CREATE TABLE src (
             path TEXT PRIMARY KEY,
             lang TEXT,
             content TEXT
           )"#,
    )?;
    Ok(db)
}

fn insert_file(db: &EmbeddedDatabase, path: &str, lang: &str, content: &str) -> Result<()> {
    let p = path.replace('\'', "''");
    let l = lang.replace('\'', "''");
    let c = content.replace('\'', "''");
    db.execute(&format!(
        "INSERT INTO src (path, lang, content) VALUES ('{p}', '{l}', '{c}')"
    ))?;
    Ok(())
}

#[test]
fn rust_lsp_definition_finds_function() -> Result<()> {
    let db = setup()?;
    insert_file(
        &db,
        "src/vector/quantization/mod.rs",
        "rust",
        "pub struct ProductQuantizer { dim: usize }\n\
         impl ProductQuantizer {\n\
             pub fn new(dim: usize) -> Self { Self { dim } }\n\
         }\n",
    )?;
    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(stats.files_parsed, 1);
    assert!(stats.symbols_written >= 2);

    let defs = db.lsp_definition("ProductQuantizer", &DefinitionHint::default())?;
    assert!(!defs.is_empty(), "lsp_definition returned no rows");
    let hit = defs
        .iter()
        .find(|d| d.path == "src/vector/quantization/mod.rs")
        .expect("expected hit in the indexed file");
    assert!(hit.signature.contains("ProductQuantizer"));
    assert!(hit.line >= 1);
    Ok(())
}

#[test]
fn lsp_references_returns_call_sites() -> Result<()> {
    let db = setup()?;
    insert_file(
        &db,
        "a.py",
        "python",
        "def helper():\n    return 42\n\ndef caller():\n    return helper() + helper()\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;

    let defs = db.lsp_definition("helper", &DefinitionHint::default())?;
    let helper_id = defs.first().expect("helper defined").symbol_id;

    let refs = db.lsp_references(helper_id)?;
    assert_eq!(refs.len(), 2, "expected two call sites, got {refs:?}");
    assert!(refs.iter().all(|r| r.kind == "CALLS"));
    Ok(())
}

#[test]
fn lsp_call_hierarchy_incoming_terminates() -> Result<()> {
    let db = setup()?;
    insert_file(
        &db,
        "chain.py",
        "python",
        "def a():\n    return b()\n\n\
         def b():\n    return c()\n\n\
         def c():\n    return 1\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;

    let defs = db.lsp_definition("c", &DefinitionHint::default())?;
    let c_id = defs.first().expect("c defined").symbol_id;
    let rows = db.lsp_call_hierarchy(
        c_id,
        heliosdb_nano::code_graph::lsp::CallDirection::Incoming,
        3,
    )?;
    let names: Vec<&str> = rows.iter().map(|r| r.qualified.as_str()).collect();
    assert!(names.contains(&"b"), "expected b as a direct caller, got {names:?}");
    Ok(())
}

#[test]
fn lsp_hover_returns_signature() -> Result<()> {
    let db = setup()?;
    insert_file(
        &db,
        "m.rs",
        "rust",
        "pub fn answer() -> i32 { 42 }\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let defs = db.lsp_definition("answer", &DefinitionHint::default())?;
    let id = defs.first().unwrap().symbol_id;
    let hover = db.lsp_hover(id)?.expect("hover row");
    assert!(hover.signature.contains("answer"));
    Ok(())
}

#[test]
fn code_index_is_idempotent() -> Result<()> {
    let db = setup()?;
    insert_file(&db, "x.rs", "rust", "pub fn f() {}\n")?;
    let s1 = db.code_index(CodeIndexOptions::for_table("src"))?;
    let s2 = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(s1.symbols_written, s2.symbols_written);
    // The second run should leave a single `f` row, not two.
    let defs = db.lsp_definition("f", &DefinitionHint::default())?;
    assert_eq!(defs.len(), 1, "re-index duplicated rows: {defs:?}");
    Ok(())
}

#[test]
fn unknown_lang_is_skipped_cleanly() -> Result<()> {
    let db = setup()?;
    insert_file(&db, "foo.cbl", "cobol", "PROGRAM FOO.\n")?;
    insert_file(&db, "ok.rs", "rust", "pub fn bar() {}\n")?;
    let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
    assert_eq!(stats.files_seen, 2);
    assert_eq!(stats.files_parsed, 1);
    assert_eq!(stats.files_skipped, 1);
    Ok(())
}
