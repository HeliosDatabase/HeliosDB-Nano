//! HTTP POST + SSE progress pairing.
//!
//! Opens an SSE stream with a known session id, then issues a
//! tools/call POST with `Mcp-Session-Id` header + progressToken.
//! Asserts the SSE stream receives at least one
//! `notifications/progress` event.

#![cfg(all(feature = "mcp-endpoint", feature = "graph-rag"))]

use std::sync::Arc;
use std::time::Duration;

use heliosdb_nano::mcp::{mcp_router, McpState};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::{json, Value};

async fn bind_router() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.graph_rag_project_symbols().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = mcp_router(McpState::new(db));
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    (addr, handle)
}

#[tokio::test]
async fn post_with_session_streams_progress_via_sse() {
    let (addr, handle) = bind_router().await;
    let session_id = format!("test-{}", uuid::Uuid::new_v4());

    // Open the SSE channel in the background and collect events.
    let sse_addr = addr;
    let sse_session = session_id.clone();
    let sse_task: tokio::task::JoinHandle<Vec<String>> = tokio::spawn(async move {
        let url = format!("http://{sse_addr}/sse?session={sse_session}");
        let resp = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .expect("sse connect");
        let mut acc: Vec<String> = Vec::new();
        let mut buf = String::new();
        // Drain the SSE stream for a bounded time; we stop once
        // we've seen the progress event.  reqwest streams bytes;
        // SSE events are separated by blank lines.
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                next = stream.next() => {
                    match next {
                        Some(Ok(chunk)) => buf.push_str(&String::from_utf8_lossy(&chunk)),
                        _ => break,
                    }
                }
            }
            // Capture each "data: ..." payload line.
            for line in buf.lines() {
                if let Some(d) = line.strip_prefix("data: ") {
                    if !acc.iter().any(|prev| prev == d) {
                        acc.push(d.to_string());
                    }
                }
            }
            if acc.iter().any(|d| d.contains("notifications/progress")) {
                break;
            }
        }
        acc
    });

    // Give the SSE handshake a tick to register the session in
    // the process-wide table before issuing the POST.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Issue the streaming POST.
    let resp: Value = reqwest::Client::new()
        .post(format!("http://{addr}/"))
        .header("Mcp-Session-Id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "helios_graphrag_search",
                "arguments": { "seed_text": "x", "hops": 1, "limit": 5 },
                "_meta": { "progressToken": "tok-http" }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 7);

    let sse_events = sse_task.await.unwrap();
    handle.abort();

    let saw_progress = sse_events
        .iter()
        .any(|e| e.contains("notifications/progress") && e.contains("tok-http"));
    assert!(
        saw_progress,
        "no notifications/progress on SSE stream; got events: {sse_events:?}"
    );
}

#[tokio::test]
async fn post_without_session_does_not_open_sse() {
    let (addr, handle) = bind_router().await;
    // No session id, no progress streaming. POST returns the
    // normal final response synchronously.
    let resp: Value = reqwest::Client::new()
        .post(format!("http://{addr}/"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "helios_graphrag_search",
                "arguments": { "seed_text": "x", "hops": 1, "limit": 5 }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["id"], 1);
    handle.abort();
}

#[tokio::test]
async fn post_with_unknown_session_falls_back_to_no_streaming() {
    let (addr, handle) = bind_router().await;
    let resp: Value = reqwest::Client::new()
        .post(format!("http://{addr}/"))
        .header("Mcp-Session-Id", "nonexistent-session-xyz")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "helios_graphrag_search",
                "arguments": { "seed_text": "x", "hops": 1, "limit": 5 },
                "_meta": { "progressToken": "tok-2" }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Final response still arrives — no streaming, no error.
    assert_eq!(resp["id"], 2);
    handle.abort();
}
