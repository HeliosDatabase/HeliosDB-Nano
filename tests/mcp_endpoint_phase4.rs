//! MCP endpoint smoke test — canonical agent handshake (`initialize` →
//! `tools/list` → `tools/call`) via the unified JSON-RPC dispatcher.
//! Transport-agnostic: the same dispatcher is used by stdio, HTTP,
//! WebSocket, and SSE mounts.

#![cfg(feature = "mcp-endpoint")]

use heliosdb_nano::mcp::{handle_rpc, handle_rpc_with_db, RpcRequest};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::{json, Value};

fn rpc(method: &str, id: i64, params: Value) -> Value {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(id)),
        method: method.into(),
        params,
    };
    serde_json::to_value(handle_rpc(req)).unwrap()
}

fn rpc_db(db: &EmbeddedDatabase, method: &str, id: i64, params: Value) -> Value {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(id)),
        method: method.into(),
        params,
    };
    serde_json::to_value(handle_rpc_with_db(db, req)).unwrap()
}

#[test]
fn canonical_mcp_handshake() {
    let init = rpc("initialize", 1, Value::Null);
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "heliosdb-nano");

    let list = rpc("tools/list", 2, Value::Null);
    let tools = list["result"]["tools"].as_array().unwrap();
    assert!(tools.len() >= 16, "expected >=16 tools, got {}", tools.len());
    for t in tools {
        assert!(t["name"].is_string());
        assert!(t["inputSchema"].is_object(), "tool {:?} missing inputSchema", t["name"]);
    }
}

#[test]
fn tools_call_invokes_in_process_tool_without_db() {
    let call = rpc(
        "tools/call",
        3,
        json!({
            "name": "heliosdb_bm25_index",
            "arguments": {
                "name": "test_index",
                "documents": [
                    { "doc_id": 1, "text": "hello world" },
                    { "doc_id": 2, "text": "goodbye world" }
                ]
            }
        }),
    );
    assert_eq!(call["result"]["isError"], false, "got {call:?}");
}

#[test]
fn tools_call_unknown_tool_surfaces_as_is_error_true() {
    let call = rpc(
        "tools/call",
        4,
        json!({ "name": "heliosdb_does_not_exist", "arguments": {} }),
    );
    assert_eq!(call["result"]["isError"], true);
}

#[test]
fn ping_returns_empty_success() {
    let pong = rpc("ping", 5, Value::Null);
    assert!(pong["result"].is_object());
    assert!(pong["error"].is_null());
}

#[test]
fn tools_call_db_tool_without_db_sets_is_error() {
    // Even though heliosdb_query exists in the catalogue, calling it
    // without a DB in context produces isError=true rather than a
    // JSON-RPC method-level error.
    let call = rpc(
        "tools/call",
        6,
        json!({ "name": "heliosdb_query", "arguments": { "sql": "SELECT 1" } }),
    );
    assert_eq!(call["result"]["isError"], true);
}

#[test]
fn tools_call_db_tool_with_db_succeeds() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT4)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    let call = rpc_db(
        &db,
        "tools/call",
        7,
        json!({ "name": "heliosdb_query", "arguments": { "sql": "SELECT id FROM t" } }),
    );
    assert_eq!(call["result"]["isError"], false, "got {call:?}");
    // Payload is JSON-encoded inside the text content.
    let inner = call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(inner.contains("\"row_count\":2"));
}

#[test]
fn resources_list_surfaces_schema_and_branches() {
    let list = rpc("resources/list", 8, Value::Null);
    let res = list["result"]["resources"].as_array().unwrap();
    let uris: Vec<_> = res.iter().filter_map(|r| r["uri"].as_str()).collect();
    assert!(uris.contains(&"heliosdb://schema"));
    assert!(uris.contains(&"heliosdb://branches"));
}

#[test]
fn resources_read_with_db_returns_schema() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT4)").unwrap();
    let read = rpc_db(
        &db,
        "resources/read",
        9,
        json!({ "uri": "heliosdb://schema" }),
    );
    let contents = read["result"]["contents"].as_array().unwrap();
    assert_eq!(contents[0]["uri"], "heliosdb://schema");
    assert!(contents[0]["text"].as_str().unwrap().contains("tables"));
}
