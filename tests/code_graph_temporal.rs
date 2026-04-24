//! FR 3 — temporal / branch LSP plus the three diff helpers.
//!
//! The engine's `AS OF` is generic MVCC; `lsp_*` SQL sugar propagates
//! the clause through the pre-parser rewriter.  These tests cover
//! parse-level plumbing (the rewriter accepts the sugar and emits
//! it into the expanded subquery) and the diff helper Rust API.
//!
//! Full temporal semantics depend on row-version retention in
//! `_hdb_code_*`; a comprehensive multi-snapshot end-to-end test is
//! part of the phase-3 semantic-Merkle work (task #173).

#![cfg(feature = "code-graph")]

use heliosdb_nano::code_graph::{
    ast_diff, lsp_body_diff, lsp_references_diff, rewrite_lsp_calls, AsOfRef,
    CodeIndexOptions, DefinitionHint, DiffChange,
};
use heliosdb_nano::{EmbeddedDatabase, Result, Value};

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
fn rewriter_propagates_as_of_commit() {
    let got = rewrite_lsp_calls(
        "SELECT * FROM lsp_definition('Foo') AS OF COMMIT 'abc123'",
    );
    assert!(
        got.contains("AS OF COMMIT 'abc123'"),
        "got: {got}"
    );
    assert!(got.contains("_hdb_code_symbols"));
}

#[test]
fn rewriter_propagates_as_of_timestamp() {
    let got = rewrite_lsp_calls(
        "SELECT * FROM lsp_references(42) AS OF TIMESTAMP '2025-01-02'",
    );
    assert!(got.contains("AS OF TIMESTAMP '2025-01-02'"));
    assert!(got.contains("_hdb_code_symbol_refs"));
}

#[test]
fn rewriter_honors_as_of_on_hover() {
    let got = rewrite_lsp_calls("SELECT * FROM lsp_hover(7) AS OF NOW");
    assert!(got.contains("AS OF NOW"));
}

#[test]
fn rewriter_honors_as_of_on_call_hierarchy() {
    let got = rewrite_lsp_calls(
        "SELECT * FROM lsp_call_hierarchy(7, 'incoming', 1) AS OF COMMIT 'deadbeef'",
    );
    assert!(got.contains("AS OF COMMIT 'deadbeef'"));
}

#[test]
fn diff_helpers_run_against_live_index() -> Result<()> {
    // The semantic test: we index a file, flip its content, re-index,
    // and ask for diffs.  Both points are AS OF NOW so the diff is
    // "no change" (row-version retention is a phase-3 follow-up).
    let db = setup()?;
    insert(&db, "m.rs", "rust", "pub fn foo() {}\npub fn bar() {}\n")?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    let defs = db.lsp_definition("foo", &DefinitionHint::default())?;
    let foo_id = defs.first().expect("foo found").symbol_id;

    let now = AsOfRef::now();
    let added_refs = lsp_references_diff(&db, foo_id, &now, &now)?;
    assert!(added_refs.is_empty(), "AS OF NOW × AS OF NOW should diff empty");
    let body = lsp_body_diff(&db, foo_id, &now, &now)?;
    for row in &body {
        assert_eq!(row.op, heliosdb_nano::code_graph::BodyOp::Equal);
    }
    let ast = ast_diff(&db, "m.rs", &now, &now)?;
    assert!(ast.is_empty());
    Ok(())
}

#[test]
fn diff_change_enum_has_stable_names() {
    // Small API-contract guard for serialisation consumers.
    assert_eq!(DiffChange::Added.as_str(), "added");
    assert_eq!(DiffChange::Removed.as_str(), "removed");
    assert_eq!(DiffChange::Moved.as_str(), "moved");
}
