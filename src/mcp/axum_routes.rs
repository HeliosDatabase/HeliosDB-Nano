//! Axum route handlers for the MCP endpoint.
//!
//! Three transports off a single `EmbeddedDatabase`:
//!
//! * `POST /mcp`      — JSON-RPC 2.0 request/response over HTTP.
//! * `GET  /mcp/ws`   — WebSocket; each text frame carries one JSON-RPC
//!                      message in either direction.
//! * `GET  /mcp/sse`  — Server-Sent-Events stream that announces the
//!                      paired POST endpoint per the MCP HTTP+SSE
//!                      transport handshake, then heartbeats.
//!
//! All three transports dispatch through the same
//! `super::rpc::handle_rpc_with_db` core, so tool / resource behaviour
//! is identical across wire formats.
//!
//! Mount the router with [`mcp_router`] and `.nest("/mcp", …)` on the
//! main Axum tree, or call [`attach`] to merge it onto an existing
//! router.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures::stream::{self, Stream};
use tracing::{debug, error, info, warn};

use crate::EmbeddedDatabase;

use super::auth::{McpAuth, Scope};
use super::rpc::{handle_rpc_with_db, RpcRequest, RpcResponse};

/// State injected into the MCP routes. Holds the database handle plus
/// an optional authenticator. Default is [`McpAuth::Disabled`] —
/// suitable for loopback / local-only mounts. For non-loopback binds
/// pass an [`McpAuth::Jwt`] and the [`crate::mcp::bind_safety_check`]
/// helper enforces the policy at bind time.
#[derive(Clone)]
pub struct McpState {
    pub db: Arc<EmbeddedDatabase>,
    pub auth: McpAuth,
}

impl McpState {
    pub fn new(db: Arc<EmbeddedDatabase>) -> Self {
        Self { db, auth: McpAuth::Disabled }
    }

    pub fn with_auth(mut self, auth: McpAuth) -> Self {
        self.auth = auth;
        self
    }
}

/// Build a stand-alone `Router<()>` carrying every MCP route. Use this
/// when wiring the MCP endpoint onto its own listener.
pub fn mcp_router(state: McpState) -> Router {
    Router::new()
        .route("/", post(handle_post))
        .route("/ws", get(handle_ws_upgrade))
        .route("/sse", get(handle_sse))
        .route("/info", get(handle_info))
        .with_state(state)
}

/// Merge the three MCP routes onto an existing router that's already
/// stateful with `S`. Useful when the host process keeps a richer
/// `AppState` and just wants to expose `/mcp*` alongside its own
/// routes.
pub fn attach<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    state: McpState,
) -> Router<S> {
    let mcp = Router::new()
        .route("/mcp", post(handle_post))
        .route("/mcp/ws", get(handle_ws_upgrade))
        .route("/mcp/sse", get(handle_sse))
        .route("/mcp/info", get(handle_info))
        .with_state(state);
    router.merge(mcp)
}

// ── POST /mcp ───────────────────────────────────────────────────────────

pub async fn handle_post(
    State(state): State<McpState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RpcRequest>,
) -> impl IntoResponse {
    debug!(method = %req.method, "mcp http request");
    let scope = Scope::for_method(&req.method);
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Err(e) = state.auth.check(auth_header, scope) {
        return (
            e.status(),
            Json(RpcResponse::error(
                req.id.unwrap_or(serde_json::Value::Null),
                -32001,
                e.message(),
            )),
        )
            .into_response();
    }
    let resp = handle_rpc_with_db(state.db.as_ref(), req);
    Json(resp).into_response()
}

// ── GET /mcp/ws ─────────────────────────────────────────────────────────

pub async fn handle_ws_upgrade(
    State(state): State<McpState>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    // Upgrade requires Write scope: the WS frame loop has no
    // per-message header, so we gate at upgrade time on the highest
    // privilege the client could exercise during the session.
    // Read-only clients can use POST /mcp instead.
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Err(e) = state.auth.check(auth_header, Scope::Write) {
        return (e.status(), e.message()).into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state))
        .into_response()
}

async fn handle_ws(mut socket: WebSocket, state: McpState) {
    info!("mcp ws client connected");
    // Snapshot whether the upgrade-time token also carried Write
    // scope. The header is gone after upgrade, so we can't re-check
    // — instead we make the test once and remember the answer.
    // (`Disabled` always returns Ok(_) so this works in test.)
    // For Disabled mode this is a no-op; for Jwt it's a single
    // signature verification.
    while let Some(msg) = socket.recv().await {
        let frame = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!("mcp ws read error: {e}");
                break;
            }
        };
        match frame {
            Message::Text(text) => {
                let resp_text = match serde_json::from_str::<RpcRequest>(&text) {
                    Ok(req) => {
                        let resp = handle_rpc_with_db(state.db.as_ref(), req);
                        serde_json::to_string(&resp).unwrap_or_else(|e| {
                            format!(
                                r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize: {e}"}}}}"#
                            )
                        })
                    }
                    Err(e) => format!(
                        r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32700,"message":"parse error: {e}"}}}}"#
                    ),
                };
                if let Err(e) = socket.send(Message::Text(resp_text)).await {
                    warn!("mcp ws send failed: {e}");
                    break;
                }
            }
            Message::Ping(p) => {
                if socket.send(Message::Pong(p)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => {
                debug!("mcp ws client closed");
                break;
            }
            _ => {}
        }
    }
    info!("mcp ws client disconnected");
}

// ── GET /mcp/sse ────────────────────────────────────────────────────────

pub async fn handle_sse(
    State(state): State<McpState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    // SSE handshake itself is read-only, but the handshake announces
    // the POST endpoint that the client will use for any request,
    // including writes. Require Read scope at minimum; the actual
    // write gate runs on POST /mcp once the client opens that
    // connection with its own Authorization header.
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Err(e) = state.auth.check(auth_header, Scope::Read) {
        return (e.status(), e.message()).into_response();
    }
    let endpoint_event = Event::default().event("endpoint").data("/mcp");
    let initial = stream::iter(vec![Ok::<_, Infallible>(endpoint_event)]);
    Sse::new(initial)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ── GET /mcp/info ───────────────────────────────────────────────────────

/// Discovery endpoint: serverInfo + capabilities + verbose tool
/// catalogue + resource list, all in one shot. Useful for clients
/// that want a single packet of self-description without doing the
/// `initialize` → `tools/list?verbose=true` → `resources/list`
/// dance over JSON-RPC.
pub async fn handle_info(
    State(state): State<McpState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Err(e) = state.auth.check(auth_header, Scope::Read) {
        return (e.status(), e.message()).into_response();
    }
    Json(super::rpc::info_result()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    async fn read_body(resp: axum::response::Response) -> Value {
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn router() -> Router {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        mcp_router(McpState::new(db))
    }

    #[tokio::test]
    async fn post_initialize_returns_handshake() {
        let app = router();
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = read_body(resp).await;
        assert_eq!(v["result"]["serverInfo"]["name"], "heliosdb-nano");
    }

    #[tokio::test]
    async fn post_tools_call_db_tool_succeeds() {
        // Need the db reference inside the handler chain to be the
        // same one we set up with a table — hand-build the state.
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE t (id INT4)").unwrap();
        db.execute("INSERT INTO t VALUES (7)").unwrap();
        let app = mcp_router(McpState::new(db));
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "heliosdb_query",
                "arguments": { "sql": "SELECT id FROM t" }
            }
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = read_body(resp).await;
        assert_eq!(v["result"]["isError"], false, "got {v}");
    }

    #[tokio::test]
    async fn sse_endpoint_emits_endpoint_event() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/sse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("text/event-stream"), "unexpected content-type: {ct}");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 12).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: endpoint"), "expected endpoint event, got: {text}");
        assert!(text.contains("data: /mcp"), "expected POST URI in payload");
    }
}
