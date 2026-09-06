//! The HTTP listener composes `ApiServer::into_router()` (BaaS: REST/Auth/Realtime/Swagger)
//! and then `attach_mcp_routes` (the authenticated, bind-safety-checked MCP mount) — exactly
//! what `src/main.rs::run_http_listener` does. This suite builds that same composition.
//!
//! Regression (sprinter 43b59beba8a9): `ApiServer::into_router` ALSO mounted its own copy of
//! `/mcp*` under the `mcp-endpoint` feature — with `McpState::new(db)` and therefore NO bearer
//! auth — so the merge in `attach_mcp_routes` panicked with
//! `Overlapping method route. Handler for POST /mcp already exists`, and every
//! `--features mcp-endpoint` binary's HTTP listener died at startup (since v4.27.0 mounted the
//! BaaS router on that listener). One route-order away from serving `/mcp` unauthenticated.

#![cfg(feature = "mcp-endpoint")]

use std::sync::Arc;

use heliosdb_nano::api::ApiServer;
use heliosdb_nano::mcp::{attach_mcp_routes, McpAuth, McpState};
use heliosdb_nano::EmbeddedDatabase;
use serde_json::{json, Value};

const TOKEN: &str = "composition-test-token";

/// The listener router as `run_http_listener` builds it (BaaS first, then authenticated MCP).
fn listener_router() -> axum::Router {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().expect("db"));
    let app = ApiServer::new("127.0.0.1:0".parse().unwrap(), Arc::clone(&db)).into_router();
    let state = McpState::new(db).with_auth(McpAuth::BearerToken(Arc::from(TOKEN)));
    attach_mcp_routes(app, state)
}

async fn spawn(app: axum::Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, handle)
}

/// Building the composition must not panic (axum panics on overlapping routes at build time).
#[test]
fn baas_plus_authenticated_mcp_composes_without_overlap() {
    let _router = listener_router();
}

/// The only `/mcp` on the listener is the authenticated one: no bearer -> 401, bearer -> a
/// spec-shaped tools/list; and the BaaS routes are still there next to it.
#[tokio::test]
async fn listener_serves_authenticated_mcp_next_to_baas() {
    let (addr, handle) = spawn(listener_router()).await;
    let client = reqwest::Client::new();
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});

    let no_auth = client
        .post(format!("http://{addr}/mcp"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(no_auth.status(), 401, "unauthenticated POST /mcp must be rejected");

    let ok = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let v: Value = ok.json().await.unwrap();
    let tools = v["result"]["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty());
    for t in tools {
        assert!(t["inputSchema"].is_object(), "tool without inputSchema: {t}");
    }

    let health = client.get(format!("http://{addr}/health")).send().await.unwrap();
    assert_eq!(
        health.status(),
        200,
        "BaaS /health must still be served on the same listener"
    );
    let version = client.get(format!("http://{addr}/version")).send().await.unwrap();
    assert_eq!(version.status(), 200);

    handle.abort();
}
