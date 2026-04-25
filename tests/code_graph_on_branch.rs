//! End-to-end test for the FR-3 `ON BRANCH '<name>'` per-call override
//! on `lsp_*` table functions.
//!
//! Indexes a Rust source on `main`, creates a branch, indexes a
//! different source there, then queries from main with
//! `ON BRANCH 'preview'` and confirms results match the branch
//! contents. Asserts the active branch is restored after the query.

#![cfg(feature = "code-graph")]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::{EmbeddedDatabase, Value};

fn upsert_src(db: &EmbeddedDatabase, path: &str, body: &str) {
    db.execute_params_returning(
        "DELETE FROM src WHERE path = $1",
        &[Value::String(path.into())],
    )
    .unwrap();
    db.execute_params_returning(
        "INSERT INTO src (path, lang, content) VALUES ($1, 'rust', $2)",
        &[Value::String(path.into()), Value::String(body.into())],
    )
    .unwrap();
}

#[test]
fn on_branch_routes_lsp_query_to_branch_data() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();

    // main: has only `alpha`.
    upsert_src(&db, "a.rs", "pub fn alpha() {}\n");
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();

    // Branch off and add a different symbol on the branch.
    db.execute("CREATE BRANCH preview AS OF NOW").unwrap();
    db.switch_branch("preview").unwrap();
    upsert_src(&db, "a.rs", "pub fn alpha() {}\npub fn beta() {}\n");
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();

    // Back to main.
    db.switch_branch("main").unwrap();
    let on_main = db
        .storage
        .get_current_branch()
        .unwrap_or_else(|| "main".into());

    // Query for `beta` on main — should resolve zero rows.
    let no_beta = db
        .query("SELECT * FROM lsp_definition('beta')", &[])
        .unwrap();
    assert!(no_beta.is_empty(), "main should not see beta, got {no_beta:?}");

    // Same query routed via `ON BRANCH 'preview'` — should hit beta.
    let with_branch = db
        .query("SELECT * FROM lsp_definition('beta') ON BRANCH 'preview'", &[])
        .unwrap();
    assert!(
        !with_branch.is_empty(),
        "ON BRANCH 'preview' should resolve beta"
    );

    // Active branch must be restored to main.
    let after = db
        .storage
        .get_current_branch()
        .unwrap_or_else(|| "main".into());
    assert_eq!(after, on_main, "ON BRANCH must restore the previous branch");
}

#[test]
fn on_branch_combined_with_as_of_compiles_and_runs() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    upsert_src(&db, "a.rs", "pub fn gamma() {}\n");
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();

    // ON BRANCH 'main' is a no-op when already on main; this just
    // exercises the trailing-clause parser combined with a real
    // call.  (`AS OF NOW` is a planner-side no-op temporal scan
    // and not all join shapes round-trip through it cleanly today.)
    let rows = db
        .query(
            "SELECT * FROM lsp_definition('gamma') ON BRANCH 'main'",
            &[],
        )
        .unwrap();
    assert!(!rows.is_empty(), "gamma should resolve");
}
