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

use super::rpc::{handle_rpc_with_db, RpcRequest, RpcResponse};

/// State injected into the MCP routes — only needs the database handle.
#[derive(Clone)]
pub struct McpState {
    pub db: Arc<EmbeddedDatabase>,
}

impl McpState {
    pub fn new(db: Arc<EmbeddedDatabase>) -> Self {
        Self { db }
    }
}

/// Build a stand-alone `Router<()>` carrying every MCP route. Use this
/// when wiring the MCP endpoint onto its own listener.
pub fn mcp_router(state: McpState) -> Router {
    Router::new()
        .route("/", post(handle_post))
        .route("/ws", get(handle_ws_upgrade))
        .route("/sse", get(handle_sse))
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
        .with_state(state);
    router.merge(mcp)
}

// ── POST /mcp ───────────────────────────────────────────────────────────

pub async fn handle_post(
    State(state): State<McpState>,
    Json(req): Json<RpcRequest>,
) -> Json<RpcResponse> {
    debug!(method = %req.method, "mcp http request");
    let resp = handle_rpc_with_db(state.db.as_ref(), req);
    Json(resp)
}

// ── GET /mcp/ws ─────────────────────────────────────────────────────────

pub async fn handle_ws_upgrade(
    State(state): State<McpState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: McpState) {
    info!("mcp ws client connected");
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
    State(_state): State<McpState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Per the MCP HTTP+SSE transport handshake, the SSE channel emits
    // an `endpoint` event whose data is the POST URI clients should
    // use for outbound JSON-RPC requests. After that we keep the
    // connection alive with comments.
    let endpoint_event = Event::default()
        .event("endpoint")
        .data("/mcp");
    let initial = stream::iter(vec![Ok::<_, Infallible>(endpoint_event)]);
    Sse::new(initial).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
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
