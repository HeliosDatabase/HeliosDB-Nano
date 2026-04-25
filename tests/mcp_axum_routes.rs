//! End-to-end smoke test for the MCP HTTP transports.
//!
//! Exercises `POST /mcp`, `GET /mcp/sse`, and the WebSocket upgrade
//! against a real `mcp_router` mounted on a TCP listener.

#![cfg(feature = "mcp-endpoint")]

use std::sync::Arc;

use heliosdb_nano::mcp::{mcp_router, McpState};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::{json, Value};

async fn bind_router() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE t (id INT4)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("INSERT INTO t VALUES (2)").unwrap();
    db.execute("INSERT INTO t VALUES (3)").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = mcp_router(McpState::new(db));
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Give the listener a moment.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, handle)
}

#[tokio::test]
async fn http_post_initialize_then_tools_call() {
    let (addr, handle) = bind_router().await;
    let client = reqwest::Client::new();

    let init_resp: Value = client
        .post(format!("http://{addr}"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "heliosdb-nano");

    let call_resp: Value = client
        .post(format!("http://{addr}"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "heliosdb_query",
                "arguments": { "sql": "SELECT id FROM t" }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(call_resp["result"]["isError"], false, "got {call_resp}");
    let inner = call_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(inner.contains("\"row_count\":3"));

    handle.abort();
}

#[tokio::test]
async fn http_sse_emits_endpoint_event() {
    let (addr, handle) = bind_router().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/sse"))
        .send()
        .await
        .unwrap();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/event-stream"), "ct={ct}");

    // Read a small slice — enough to see the endpoint event then drop.
    let bytes = resp.bytes().await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("event: endpoint"), "no endpoint event: {text}");
    assert!(text.contains("data: /mcp"), "no POST URI: {text}");

    handle.abort();
}

#[tokio::test]
async fn websocket_round_trip() {
    let (addr, handle) = bind_router().await;

    let url = format!("ws://{addr}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    use futures::{SinkExt, StreamExt};
    let req = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/list",
        "params": {}
    });
    ws.send(tokio_tungstenite::tungstenite::Message::Text(req.to_string()))
        .await
        .unwrap();
    let resp = ws.next().await.expect("response").unwrap();
    let text = resp.into_text().unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let tools = v["result"]["tools"].as_array().unwrap();
    assert!(tools.len() >= 16, "expected >=16 tools, got {}", tools.len());

    handle.abort();
}
