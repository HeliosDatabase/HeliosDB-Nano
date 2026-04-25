//! End-to-end auth tests for the MCP HTTP transport.
//!
//! Spins up a real `mcp_router` with `McpAuth::Jwt(...)` and verifies:
//! * unauthenticated requests get 401
//! * read-scoped tokens succeed on `tools/list` but get 403 on `tools/call`
//! * write-scoped tokens succeed on both
//! * `bind_safety_check` refuses non-loopback binds when auth is disabled

#![cfg(feature = "mcp-endpoint")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use heliosdb_nano::api::jwt::JwtManager;
use heliosdb_nano::mcp::{bind_safety_check, mcp_router, McpAuth, McpState};
use heliosdb_nano::EmbeddedDatabase;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};

fn make_token(secret: &[u8], scopes: &[&str]) -> String {
    let mgr = JwtManager::new(secret);
    let raw = mgr
        .generate_token(
            "u".into(),
            "t".into(),
            uuid::Uuid::new_v4(),
        )
        .unwrap();
    let mut claims = mgr.validate_token(&raw).unwrap();
    claims.scopes = scopes.iter().map(|s| (*s).to_string()).collect();
    encode(&Header::default(), &claims, &EncodingKey::from_secret(secret)).unwrap()
}

async fn bind_with_auth(auth: McpAuth) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE t (id INT4)").unwrap();
    let state = McpState::new(db).with_auth(auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = mcp_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, handle)
}

#[tokio::test]
async fn missing_token_returns_401() {
    let mgr = Arc::new(JwtManager::new(b"secret"));
    let (addr, handle) = bind_with_auth(McpAuth::Jwt(mgr)).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    handle.abort();
}

#[tokio::test]
async fn read_scope_passes_tools_list() {
    let mgr = Arc::new(JwtManager::new(b"secret"));
    let token = make_token(b"secret", &["mcp:read"]);
    let (addr, handle) = bind_with_auth(McpAuth::Jwt(mgr)).await;
    let resp: Value = reqwest::Client::new()
        .post(format!("http://{addr}"))
        .bearer_auth(token)
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp["result"]["tools"].is_array(), "got {resp}");
    handle.abort();
}

#[tokio::test]
async fn read_scope_fails_tools_call() {
    let mgr = Arc::new(JwtManager::new(b"secret"));
    let token = make_token(b"secret", &["mcp:read"]);
    let (addr, handle) = bind_with_auth(McpAuth::Jwt(mgr)).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}"))
        .bearer_auth(token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "heliosdb_query", "arguments": { "sql": "SELECT 1" } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    handle.abort();
}

#[tokio::test]
async fn write_scope_passes_tools_call() {
    let mgr = Arc::new(JwtManager::new(b"secret"));
    let token = make_token(b"secret", &["mcp:read", "mcp:write"]);
    let (addr, handle) = bind_with_auth(McpAuth::Jwt(mgr)).await;
    let resp: Value = reqwest::Client::new()
        .post(format!("http://{addr}"))
        .bearer_auth(token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "heliosdb_query", "arguments": { "sql": "SELECT 1" } }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["result"]["isError"], false, "got {resp}");
    handle.abort();
}

#[test]
fn bind_safety_loopback_with_disabled_ok() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
    assert!(bind_safety_check(addr, &McpAuth::Disabled).is_ok());
}

#[test]
fn bind_safety_unspecified_disabled_refused() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080);
    let err = bind_safety_check(addr, &McpAuth::Disabled).unwrap_err();
    assert!(err.contains("non-loopback"), "got: {err}");
}

#[test]
fn bind_safety_unspecified_jwt_ok() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080);
    let auth = McpAuth::Jwt(Arc::new(JwtManager::new(b"strong-secret")));
    assert!(bind_safety_check(addr, &auth).is_ok());
}
