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

    // Streaming progress over the paired SSE channel: opt in by
    // sending both `Mcp-Session-Id` (matching an open SSE
    // connection) AND `_meta.progressToken`. Any tools/call meeting
    // both gets its progress events forwarded to the SSE stream
    // while the POST itself returns the final response.
    if req.method == "tools/call" {
        let session_id = headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let token = extract_progress_token(&req.params);
        if let (Some(sid), Some(tok)) = (session_id, token) {
            if let Some(sse_tx) = super::session::sender_for(&sid) {
                let resp = dispatch_streaming_post(state.db.clone(), req, tok, sse_tx).await;
                return Json(resp).into_response();
            }
        }
    }

    let resp = handle_rpc_with_db(state.db.as_ref(), req);
    Json(resp).into_response()
}

/// Streaming-progress dispatch over the SSE-paired POST: forward
/// every `notifications/progress` event to the SSE channel keyed
/// by the `Mcp-Session-Id` header, while the POST waits for the
/// final tools/call response.
async fn dispatch_streaming_post(
    db: std::sync::Arc<crate::EmbeddedDatabase>,
    req: RpcRequest,
    progress_token: serde_json::Value,
    sse_tx: tokio::sync::mpsc::UnboundedSender<axum::response::sse::Event>,
) -> RpcResponse {
    use super::streaming::call_tool_streaming;
    let id = req.id.clone().unwrap_or(serde_json::Value::Null);
    let name = req
        .params
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let args = req
        .params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let Some(name) = name else {
        return RpcResponse::error(id, -32000, "tools/call requires 'name'");
    };

    let (mut rx, handle) = call_tool_streaming(Some(db), name, args);
    while let Some(ev) = rx.recv().await {
        let mut params = serde_json::json!({
            "progressToken": progress_token,
            "progress": ev.progress,
        });
        if let Some(total) = ev.total {
            if let Some(o) = params.as_object_mut() {
                o.insert("total".into(), serde_json::Value::from(total));
            }
        }
        if let Some(msg) = ev.message {
            if let Some(o) = params.as_object_mut() {
                o.insert("message".into(), serde_json::Value::String(msg));
            }
        }
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        });
        let event = axum::response::sse::Event::default()
            .event("notifications/progress")
            .data(payload.to_string());
        if sse_tx.send(event).is_err() {
            // SSE channel dropped — keep the tool running but stop
            // forwarding events.
            break;
        }
    }
    let outcome = handle
        .await
        .unwrap_or_else(|e| super::tools::ToolOutcome::err(format!("tool task panicked: {e}")));
    RpcResponse::success(
        id,
        serde_json::json!({
            "isError": outcome.is_error,
            "content": [
                { "type": "text", "text": outcome.payload.to_string() }
            ]
        }),
    )
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
                if !dispatch_ws_text(&mut socket, &state, &text).await {
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

/// Returns `true` if the connection should stay open, `false` if
/// downstream sends failed and we should disconnect.
async fn dispatch_ws_text(socket: &mut WebSocket, state: &McpState, text: &str) -> bool {
    let parsed: serde_json::Result<RpcRequest> = serde_json::from_str(text);
    let req = match parsed {
        Ok(r) => r,
        Err(e) => {
            let err = format!(
                r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32700,"message":"parse error: {e}"}}}}"#
            );
            return socket.send(Message::Text(err)).await.is_ok();
        }
    };

    // tools/call with a progressToken takes the streaming path —
    // the handler runs on a blocking thread, progress events are
    // forwarded as `notifications/progress`, then the final
    // tools/call response is sent.
    if req.method == "tools/call" {
        if let Some(token) = extract_progress_token(&req.params) {
            return dispatch_streaming_tools_call(socket, state, req, token).await;
        }
    }

    let resp = super::rpc::handle_rpc_with_db(state.db.as_ref(), req);
    let json = serde_json::to_string(&resp).unwrap_or_else(|e| {
        format!(
            r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize: {e}"}}}}"#
        )
    });
    socket.send(Message::Text(json)).await.is_ok()
}

/// Per the MCP spec: `params._meta.progressToken` (string or number)
/// signals the client wants progress notifications. Anything else
/// means no streaming.
fn extract_progress_token(params: &serde_json::Value) -> Option<serde_json::Value> {
    let token = params.get("_meta")?.get("progressToken")?;
    if token.is_string() || token.is_number() {
        Some(token.clone())
    } else {
        None
    }
}

async fn dispatch_streaming_tools_call(
    socket: &mut WebSocket,
    state: &McpState,
    req: RpcRequest,
    progress_token: serde_json::Value,
) -> bool {
    use super::streaming::call_tool_streaming;

    // Pull tool name + arguments from the JSON-RPC params shape.
    let name = req
        .params
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let args = req.params.get("arguments").cloned().unwrap_or(serde_json::Value::Null);

    let id = req.id.clone().unwrap_or(serde_json::Value::Null);
    let Some(name) = name else {
        let err = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": "tools/call requires 'name'" }
        });
        return socket
            .send(Message::Text(err.to_string()))
            .await
            .is_ok();
    };

    let (mut rx, handle) = call_tool_streaming(Some(state.db.clone()), name, args);

    // Forward progress events as JSON-RPC notifications until the
    // handler completes.
    while let Some(ev) = rx.recv().await {
        let mut params = serde_json::json!({
            "progressToken": progress_token,
            "progress": ev.progress,
        });
        if let Some(total) = ev.total {
            if let Some(obj) = params.as_object_mut() {
                obj.insert("total".into(), serde_json::Value::from(total));
            }
        }
        if let Some(msg) = ev.message {
            if let Some(obj) = params.as_object_mut() {
                obj.insert("message".into(), serde_json::Value::String(msg));
            }
        }
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        });
        if socket
            .send(Message::Text(notif.to_string()))
            .await
            .is_err()
        {
            return false;
        }
    }

    let outcome = handle.await.unwrap_or_else(|e| super::tools::ToolOutcome::err(format!("tool task panicked: {e}")));
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "isError": outcome.is_error,
            "content": [
                { "type": "text", "text": outcome.payload.to_string() }
            ]
        }
    });
    socket.send(Message::Text(resp.to_string())).await.is_ok()
}

// ── GET /mcp/sse ────────────────────────────────────────────────────────

pub async fn handle_sse(
    State(state): State<McpState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
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

    // Resolve the session id: prefer the client's `?session=<id>`
    // query param, otherwise mint a fresh UUID. We announce the
    // session id via the spec-defined `endpoint` event so the
    // client knows what `Mcp-Session-Id` header to set on the
    // paired POST /mcp.
    let session_id = params
        .get("session")
        .cloned()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let rx = super::session::register(session_id.clone());

    // The endpoint event payload mirrors the MCP HTTP+SSE handshake
    // shape: a URL the client should POST to.  We include the
    // session id as a query suffix so naive clients parse the URL
    // and copy it back.  Spec-aware clients use the
    // `Mcp-Session-Id` header explicitly.
    let endpoint_data = format!("/mcp?session={session_id}");
    let endpoint_event = Event::default().event("endpoint").data(endpoint_data);

    // Forward every event the session sender emits as an SSE
    // chunk, then end-of-stream when the channel is closed (the
    // session is dropped from the table on TTL expiry).  Built via
    // `futures::stream::unfold` to avoid pulling in tokio-stream.
    let progress_stream = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(ev) => Some((Ok::<_, Infallible>(ev), rx)),
            None => None,
        }
    });
    let initial = stream::iter(vec![Ok::<_, Infallible>(endpoint_event)]);
    use futures::StreamExt;
    let combined = initial.chain(progress_stream);

    Sse::new(combined)
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
