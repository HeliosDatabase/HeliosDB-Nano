//! Integration test for the `mcp_tool!` auto-registration surface.
//!
//! Verifies the LSP-shaped Rust functions in `code_graph::lsp` reach
//! the MCP catalogue without any edits to `tools.rs` — declared
//! once via `inventory::submit!` in `src/mcp/lsp_tools.rs`, found by
//! `tools::list_tools`, and dispatchable via `tools::call_tool`.

#![cfg(all(feature = "mcp-endpoint", feature = "code-graph"))]

use heliosdb_nano::code_graph::CodeIndexOptions;
use heliosdb_nano::mcp::{call_tool, list_tools};
use heliosdb_nano::{EmbeddedDatabase, Value};
use serde_json::json;

#[test]
fn lsp_tools_appear_in_list() {
    let names: Vec<_> = list_tools().into_iter().map(|t| t.name).collect();
    for n in [
        "helios_lsp_definition",
        "helios_lsp_references",
        "helios_lsp_call_hierarchy",
        "helios_lsp_hover",
    ] {
        assert!(names.contains(&n), "missing {n} in {names:?}");
    }
}

#[test]
fn lsp_definition_via_mcp_resolves_indexed_function() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")
        .unwrap();
    db.execute_params_returning(
        "INSERT INTO src VALUES ($1, 'rust', $2)",
        &[
            Value::String("a.rs".to_string()),
            Value::String("pub fn greet() -> String { String::from(\"hi\") }\n".to_string()),
        ],
    )
    .unwrap();
    db.code_index(CodeIndexOptions::for_table("src")).unwrap();

    let r = call_tool(
        Some(&db),
        "helios_lsp_definition",
        json!({ "name": "greet" }),
    );
    assert!(!r.is_error, "got {r:?}");
    let rows = r.payload["rows"].as_array().expect("rows");
    assert!(!rows.is_empty(), "no rows for 'greet': {:?}", r.payload);
    let first = &rows[0];
    assert_eq!(first["path"].as_str(), Some("a.rs"));
}

#[test]
fn lsp_definition_without_db_errors_cleanly() {
    let r = call_tool(None, "helios_lsp_definition", json!({ "name": "x" }));
    assert!(r.is_error);
    assert!(r.payload["error"]
        .as_str()
        .unwrap()
        .contains("requires a database"));
}

#[test]
fn unknown_lsp_tool_falls_through_to_unknown() {
    let r = call_tool(None, "helios_lsp_definitelynot", json!({}));
    assert!(r.is_error);
    let msg = r.payload["error"].as_str().unwrap();
    assert!(msg.contains("unknown tool"), "got: {msg}");
}
