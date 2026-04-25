//! MCP 2024-11-05 conformance handshake test.
//!
//! Drives the canonical client flow against the unified RPC
//! dispatcher and verifies every response shape against the
//! protocol's required fields.  Runs in-process so we don't have
//! to start a real server / boot a Docker image.
//!
//! Spec reference: <https://modelcontextprotocol.io/specification/2024-11-05/>

#![cfg(feature = "mcp-endpoint")]

use heliosdb_nano::mcp::{handle_rpc, handle_rpc_with_db, RpcRequest};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::{json, Value};

fn req(id: i64, method: &str, params: Value) -> RpcRequest {
    RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(id)),
        method: method.into(),
        params,
    }
}

fn assert_jsonrpc_envelope(resp: &Value) {
    assert_eq!(resp["jsonrpc"], "2.0", "every response must carry jsonrpc=2.0");
    assert!(resp["id"].is_number() || resp["id"].is_string() || resp["id"].is_null());
    let has_result = resp.get("result").map_or(false, |v| !v.is_null());
    let has_error = resp.get("error").map_or(false, |v| !v.is_null());
    assert!(
        has_result ^ has_error,
        "response must have exactly one of result/error: got {resp}"
    );
}

#[test]
fn initialize_handshake_carries_required_fields() {
    let resp = serde_json::to_value(handle_rpc(req(
        1,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "conformance-test", "version": "1.0" }
        }),
    )))
    .unwrap();
    assert_jsonrpc_envelope(&resp);
    let r = &resp["result"];
    assert_eq!(r["protocolVersion"], "2024-11-05");
    assert!(r["serverInfo"]["name"].is_string());
    assert!(r["serverInfo"]["version"].is_string());
    let caps = &r["capabilities"];
    assert!(
        caps["tools"].is_object(),
        "server must advertise tools capability"
    );
    assert!(
        caps["resources"].is_object(),
        "server must advertise resources capability"
    );
}

#[test]
fn tools_list_response_shape() {
    let resp =
        serde_json::to_value(handle_rpc(req(2, "tools/list", json!({})))).unwrap();
    assert_jsonrpc_envelope(&resp);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty(), "server must advertise at least one tool");
    for t in tools {
        // Spec-required fields per Tool schema.
        assert!(t["name"].is_string(), "tool missing name: {t}");
        assert!(t["description"].is_string(), "tool missing description: {t}");
        assert!(t["inputSchema"].is_object(), "tool missing inputSchema: {t}");
        // inputSchema must be a JSON Schema object.
        let schema = &t["inputSchema"];
        assert_eq!(
            schema["type"].as_str(),
            Some("object"),
            "inputSchema.type must be 'object': {t}"
        );
    }
}

#[test]
fn tools_call_response_shape() {
    let resp = serde_json::to_value(handle_rpc(req(
        3,
        "tools/call",
        json!({
            "name": "heliosdb_bm25_index",
            "arguments": {
                "name": "conformance-bm25",
                "documents": [
                    { "doc_id": 1, "text": "alpha" },
                    { "doc_id": 2, "text": "beta" }
                ]
            }
        }),
    )))
    .unwrap();
    assert_jsonrpc_envelope(&resp);
    let r = &resp["result"];
    // Spec-required: result.isError + result.content array.
    assert!(r["isError"].is_boolean(), "tools/call result must carry isError");
    let content = r["content"].as_array().expect("content array");
    assert!(!content.is_empty());
    for c in content {
        assert!(c["type"].is_string(), "content item missing type: {c}");
    }
}

#[test]
fn resources_list_response_shape() {
    let resp =
        serde_json::to_value(handle_rpc(req(4, "resources/list", json!({})))).unwrap();
    assert_jsonrpc_envelope(&resp);
    let resources = resp["result"]["resources"]
        .as_array()
        .expect("resources array");
    for r in resources {
        assert!(r["uri"].is_string(), "resource missing uri: {r}");
        assert!(r["name"].is_string(), "resource missing name: {r}");
    }
}

#[test]
fn resources_read_response_shape() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT4)").unwrap();
    let resp = serde_json::to_value(handle_rpc_with_db(
        &db,
        req(5, "resources/read", json!({ "uri": "heliosdb://schema" })),
    ))
    .unwrap();
    assert_jsonrpc_envelope(&resp);
    let contents = resp["result"]["contents"].as_array().expect("contents");
    assert!(!contents.is_empty());
    for c in contents {
        assert!(c["uri"].is_string(), "content missing uri: {c}");
        // mimeType + (text | blob) is the required pair.
        assert!(c["mimeType"].is_string(), "content missing mimeType: {c}");
        let has_text = c.get("text").map_or(false, |v| v.is_string());
        let has_blob = c.get("blob").map_or(false, |v| v.is_string());
        assert!(has_text || has_blob, "content missing text/blob: {c}");
    }
}

#[test]
fn ping_returns_empty_object() {
    let resp = serde_json::to_value(handle_rpc(req(6, "ping", json!({})))).unwrap();
    assert_jsonrpc_envelope(&resp);
    assert!(resp["result"].is_object());
}

#[test]
fn unknown_method_returns_method_not_found() {
    let resp = serde_json::to_value(handle_rpc(req(7, "this/does/not/exist", json!({}))))
        .unwrap();
    let err = &resp["error"];
    assert!(err.is_object(), "unknown method must produce an error envelope");
    assert_eq!(err["code"].as_i64(), Some(-32601));
    assert!(err["message"].is_string());
}

#[test]
fn invalid_tools_call_params_returns_application_error() {
    // tools/call without a name violates the tool schema; the
    // dispatcher returns an application error code (-32000), not
    // -32601.
    let resp = serde_json::to_value(handle_rpc(req(8, "tools/call", json!({})))).unwrap();
    let err = &resp["error"];
    assert_eq!(err["code"].as_i64(), Some(-32000));
}
