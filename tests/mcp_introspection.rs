//! Discovery surface tests:
//!   * `tools/list` honors the `verbose` flag (per-tool category +
//!     requiresDatabase)
//!   * `helios/info` JSON-RPC method returns a single discovery
//!     packet
//!   * `GET /mcp/info` HTTP route returns the same payload

#![cfg(feature = "mcp-endpoint")]

use std::sync::Arc;

use heliosdb_nano::mcp::{handle_rpc, mcp_router, McpState, RpcRequest};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::{json, Value};

fn rpc_call(method: &str, params: Value) -> Value {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params,
    };
    serde_json::to_value(handle_rpc(req)).unwrap()
}

#[test]
fn tools_list_default_omits_extra_fields() {
    let resp = rpc_call("tools/list", Value::Null);
    let tools = resp["result"]["tools"].as_array().unwrap();
    let first = &tools[0];
    assert!(first["name"].is_string());
    assert!(first["inputSchema"].is_object());
    // No extra fields when verbose isn't set.
    assert!(first.get("category").is_none());
    assert!(first.get("requiresDatabase").is_none());
}

#[test]
fn tools_list_verbose_adds_category_and_db_flag() {
    let resp = rpc_call("tools/list", json!({ "verbose": true }));
    let tools = resp["result"]["tools"].as_array().unwrap();
    for t in tools {
        assert!(t["category"].is_string(), "missing category on {t}");
        assert!(t["requiresDatabase"].is_boolean(), "missing requiresDatabase on {t}");
    }
    // The 6 in-process RAG tools must be flagged requiresDatabase=false.
    let names_without_db: Vec<_> = tools
        .iter()
        .filter(|t| t["requiresDatabase"].as_bool() == Some(false))
        .filter_map(|t| t["name"].as_str())
        .collect();
    for n in [
        "heliosdb_bm25_index",
        "heliosdb_hybrid_search",
        "heliosdb_graph_add_edge",
        "heliosdb_graph_traverse",
        "heliosdb_graph_path",
        "heliosdb_embed_and_store",
    ] {
        assert!(names_without_db.contains(&n), "{n} should be marked DB-free; have: {names_without_db:?}");
    }
}

#[test]
fn helios_info_rpc_returns_single_discovery_packet() {
    let resp = rpc_call("helios/info", Value::Null);
    let r = &resp["result"];
    assert_eq!(r["serverInfo"]["name"], "heliosdb-nano");
    assert!(r["protocolVersion"].is_string());
    let tools = r["tools"].as_array().expect("tools");
    let resources = r["resources"].as_array().expect("resources");
    assert!(!tools.is_empty());
    assert!(!resources.is_empty());
    // Verbose fields are present in the embedded tool list.
    assert!(tools[0]["category"].is_string());
}

#[tokio::test]
async fn http_get_mcp_info_returns_discovery_payload() {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = mcp_router(McpState::new(db));
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let info: Value = reqwest::get(format!("http://{addr}/info"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["serverInfo"]["name"], "heliosdb-nano");
    assert!(info["tools"].as_array().unwrap().len() >= 16);

    handle.abort();
}
