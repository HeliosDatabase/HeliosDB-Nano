//! Phase-4 MVP: MCP JSON-RPC endpoint smoke test. Covers the canonical
//! agent handshake (`initialize` → `tools/list` → `tools/call`) without
//! needing an Axum server in the loop — the dispatcher is pure.
//!
//! Feature flag `mcp-endpoint`. Axum route wiring is left to the
//! embedder; this test verifies the RPC surface.

#![cfg(feature = "mcp-endpoint")]

use heliosdb_nano::mcp_http::{handle_rpc, RpcRequest};
use serde_json::{json, Value};

fn rpc(method: &str, id: i64, params: Value) -> serde_json::Value {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(id)),
        method: method.into(),
        params,
    };
    serde_json::to_value(handle_rpc(req)).unwrap()
}

#[test]
fn canonical_mcp_handshake() {
    let init = rpc("initialize", 1, Value::Null);
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "heliosdb-nano");

    let list = rpc("tools/list", 2, Value::Null);
    let tools = list["result"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty(), "tools/list returned zero tools");
    // Every tool must carry an inputSchema — agents can't call it otherwise.
    for t in tools {
        assert!(t["name"].is_string());
        assert!(t["inputSchema"].is_object(), "tool {:?} missing inputSchema", t["name"]);
    }
}

#[test]
fn tools_call_invokes_a_real_tool() {
    // Pick a tool that's always safe — bm25_index with an empty
    // document list. call_tool returns a success payload.
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
    assert!(call["result"].is_object(), "expected result, got {call:?}");
    assert_eq!(call["result"]["isError"], false, "got {call:?}");
}

#[test]
fn tools_call_unknown_tool_surfaces_as_isError_true() {
    let call = rpc(
        "tools/call",
        4,
        json!({
            "name": "heliosdb_does_not_exist",
            "arguments": {}
        }),
    );
    // call_tool returns an error outcome, which we wrap as
    // isError=true. Note this is distinct from a JSON-RPC -32601
    // (which is for unknown RPC methods, not unknown tool names).
    assert_eq!(call["result"]["isError"], true);
}

#[test]
fn ping_returns_empty_success() {
    let pong = rpc("ping", 5, Value::Null);
    assert!(pong["result"].is_object());
    assert!(pong["error"].is_null());
}
