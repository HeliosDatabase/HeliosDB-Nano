//! Integration tests for the RAG-native MCP idea-5 tools.
//!
//! See `BLOCKER_idea_5.md` for the rationale behind testing against
//! `mcp_extensions` rather than the legacy `mcp` module.

use heliosdb_nano::mcp_extensions::{call_tool, list_tools, read_resource};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::json;
use uuid::Uuid;

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

#[test]
fn six_new_tools_are_listed() {
    let names: Vec<_> = list_tools().into_iter().map(|t| t.name).collect();
    for n in [
        "heliosdb_bm25_index",
        "heliosdb_hybrid_search",
        "heliosdb_graph_add_edge",
        "heliosdb_graph_traverse",
        "heliosdb_graph_path",
        "heliosdb_embed_and_store",
    ] {
        assert!(names.contains(&n), "missing: {n} -- have: {names:?}");
    }
}

#[test]
fn bm25_index_then_hybrid_search_promotes_co_occurring_doc() {
    let name = unique("integration-bm25");
    let r = call_tool(
        "heliosdb_bm25_index",
        json!({
            "name": name,
            "documents": [
                {"doc_id": 1, "text": "rust async io"},
                {"doc_id": 2, "text": "python pandas data"},
                {"doc_id": 3, "text": "rust embeddings vector search"},
            ]
        }),
    );
    assert!(!r.is_error, "indexing failed: {:?}", r.payload);

    let r2 = call_tool(
        "heliosdb_hybrid_search",
        json!({
            "index_name": name,
            "query_text": "embeddings vector",
            "vector_hits": [
                {"doc_id": 3, "score": 0.95},
                {"doc_id": 1, "score": 0.6}
            ],
            "fusion": "rrf",
            "limit": 5
        }),
    );
    assert!(!r2.is_error, "search failed: {:?}", r2.payload);
    let arr = r2.payload["results"].as_array().expect("results");
    // Doc 3 is the only doc that matches both query terms AND tops
    // the vector list -> unambiguous winner.
    assert_eq!(arr.first().and_then(|v| v["doc_id"].as_u64()), Some(3));
}

#[test]
fn graph_add_traverse_path_roundtrip() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    for (from, to) in [(a, b), (b, c)] {
        let r = call_tool(
            "heliosdb_graph_add_edge",
            json!({"from": from.to_string(), "to": to.to_string(), "label": "x", "weight": 1.0}),
        );
        assert!(!r.is_error);
    }

    let trav = call_tool(
        "heliosdb_graph_traverse",
        json!({"start": a.to_string(), "edge_label": "x", "depth": 5}),
    );
    assert!(!trav.is_error);
    let rows = trav.payload["rows"].as_array().expect("rows");
    assert!(rows.len() >= 3);

    let path = call_tool(
        "heliosdb_graph_path",
        json!({
            "from": a.to_string(),
            "to": c.to_string(),
            "algorithm": "dijkstra",
            "edge_label": "x"
        }),
    );
    assert!(!path.is_error);
    assert_eq!(path.payload["hops"].as_u64(), Some(2));
}

#[test]
fn graph_path_returns_not_found_for_unconnected_nodes() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let r = call_tool(
        "heliosdb_graph_path",
        json!({"from": a.to_string(), "to": b.to_string()}),
    );
    assert!(!r.is_error);
    assert_eq!(r.payload["path_found"].as_bool(), Some(false));
}

#[test]
fn embed_and_store_then_search_finds_doc() {
    let name = unique("integration-embed");
    let r = call_tool(
        "heliosdb_embed_and_store",
        json!({"index_name": name, "doc_id": 42, "text": "the quick brown fox"}),
    );
    assert!(!r.is_error);

    let r2 = call_tool(
        "heliosdb_hybrid_search",
        json!({"index_name": name, "query_text": "fox", "fusion": "rrf", "limit": 5}),
    );
    let arr = r2.payload["results"].as_array().expect("results");
    assert!(arr.iter().any(|h| h["doc_id"].as_u64() == Some(42)));
}

#[test]
fn schema_and_stats_resources_resolve() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let s = read_resource(&db, "heliosdb://schema/users")
        .expect("matched")
        .expect("ok");
    assert_eq!(s.mime_type, "application/json");
    assert!(s.text.contains("users"));

    let st = read_resource(&db, "heliosdb://stats/orders")
        .expect("matched")
        .expect("ok");
    assert!(st.text.contains("orders"));
    assert!(read_resource(&db, "heliosdb://nope/x").is_none());
}
