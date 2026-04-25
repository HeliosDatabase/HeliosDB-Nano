//! Integration tests for the post-merge follow-up MCP tools:
//!   - helios_graphrag_search       (#3)
//!   - helios_lsp_diff / ast_diff   (#4)
//!   - helios_lsp_document_symbols  (#1)
//!   - helios_lsp_rename_preview    (#2)

#![cfg(all(feature = "mcp-endpoint", feature = "graph-rag"))]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::mcp::{call_tool, list_tools};
use heliosdb_nano::{EmbeddedDatabase, Value};
use serde_json::json;

fn indexed_db() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'rust', $2)",
        &[
            Value::String("a.rs".to_string()),
            Value::String(
                "pub fn alpha() {}\npub fn beta() { alpha(); }\n".to_string(),
            ),
        ],
    )
    .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();
    db
}

#[test]
fn all_followup_tools_are_listed() {
    let names: Vec<_> = list_tools().into_iter().map(|t| t.name).collect();
    for n in [
        "helios_graphrag_search",
        "helios_lsp_document_symbols",
        "helios_lsp_rename_preview",
        "helios_lsp_references_diff",
        "helios_lsp_body_diff",
        "helios_ast_diff",
    ] {
        assert!(names.contains(&n), "missing {n} in {names:?}");
    }
}

#[test]
fn document_symbols_returns_file_outline() {
    let db = indexed_db();
    let r = call_tool(
        Some(&db),
        "helios_lsp_document_symbols",
        json!({ "path": "a.rs" }),
    );
    assert!(!r.is_error, "{r:?}");
    let symbols = r.payload["symbols"].as_array().expect("symbols");
    let names: Vec<_> = symbols
        .iter()
        .filter_map(|s| s.get("name").and_then(|v| v.as_str()))
        .collect();
    assert!(names.contains(&"alpha"), "have: {names:?}");
    assert!(names.contains(&"beta"), "have: {names:?}");
}

#[test]
fn document_symbols_filters_by_kind() {
    let db = indexed_db();
    let r = call_tool(
        Some(&db),
        "helios_lsp_document_symbols",
        json!({ "path": "a.rs", "kinds": ["function"] }),
    );
    assert!(!r.is_error);
    // a.rs has only functions; the filter should not drop anything.
    let count = r.payload["count"].as_u64().unwrap();
    assert!(count >= 2, "expected ≥2 functions, got {count}");
}

#[test]
fn rename_preview_collects_definition_and_refs() {
    let db = indexed_db();
    // Look up alpha's symbol_id via lsp_definition.
    let def = call_tool(Some(&db), "helios_lsp_definition", json!({ "name": "alpha" }));
    assert!(!def.is_error, "{def:?}");
    let symbol_id = def.payload["rows"][0]["symbol_id"].as_i64().expect("id");

    let r = call_tool(
        Some(&db),
        "helios_lsp_rename_preview",
        json!({ "symbol_id": symbol_id, "new_name": "alpha2" }),
    );
    assert!(!r.is_error, "{r:?}");
    assert_eq!(r.payload["found"], true);
    assert_eq!(r.payload["applied"], false);
    let edits = r.payload["edits"].as_array().expect("edits");
    // At least the definition edit. Whether call sites resolve back to
    // a non-NULL caller_symbol depends on resolver heuristics, but the
    // definition row is always present.
    assert!(!edits.is_empty(), "expected ≥1 edit");
    let first = &edits[0];
    assert_eq!(first["kind"], "definition");
    assert_eq!(first["old_name"], "alpha");
    assert_eq!(first["new_name"], "alpha2");
}

#[test]
fn rename_preview_unknown_symbol_returns_found_false() {
    let db = indexed_db();
    let r = call_tool(
        Some(&db),
        "helios_lsp_rename_preview",
        json!({ "symbol_id": 9999999, "new_name": "x" }),
    );
    assert!(!r.is_error);
    assert_eq!(r.payload["found"], false);
}

#[test]
fn graphrag_search_matches_indexed_symbol() {
    let db = indexed_db();
    // The phase-3 trigger projects code symbols into _hdb_graph_nodes
    // automatically, so seeding by the symbol's qualified/title text
    // resolves cross-modally.
    db.graph_rag_project_symbols().unwrap();
    let r = call_tool(
        Some(&db),
        "helios_graphrag_search",
        json!({ "seed_text": "alpha", "hops": 1, "limit": 20 }),
    );
    assert!(!r.is_error, "{r:?}");
    let count = r.payload["count"].as_u64().unwrap();
    assert!(count >= 1, "expected ≥1 hit, got {count}");
    let titles: Vec<_> = r.payload["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["title"].as_str())
        .collect();
    assert!(
        titles.iter().any(|t| t.contains("alpha")),
        "expected an 'alpha' hit; titles: {titles:?}"
    );
}

#[test]
fn graphrag_search_empty_seed_errors() {
    let db = indexed_db();
    let r = call_tool(
        Some(&db),
        "helios_graphrag_search",
        json!({ "seed_text": "" }),
    );
    assert!(r.is_error);
}

#[test]
fn ast_diff_now_vs_now_is_empty() {
    let db = indexed_db();
    let r = call_tool(
        Some(&db),
        "helios_ast_diff",
        json!({ "path": "a.rs", "at_a": "now", "at_b": "now" }),
    );
    assert!(!r.is_error, "{r:?}");
    let count = r.payload["count"].as_u64().unwrap();
    assert_eq!(count, 0, "now-vs-now should produce zero diff rows");
}

#[test]
fn body_diff_shape_validates_as_of() {
    let db = indexed_db();
    let r = call_tool(
        Some(&db),
        "helios_lsp_body_diff",
        json!({ "symbol_id": 1, "at_a": { "garbage": true } }),
    );
    assert!(r.is_error);
    let msg = r.payload["error"].as_str().unwrap();
    assert!(msg.contains("as_of"), "got: {msg}");
}
