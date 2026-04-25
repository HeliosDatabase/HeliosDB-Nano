//! Stdio MCP server.
//!
//! Reads JSON-RPC 2.0 requests line-by-line from stdin, dispatches via
//! the shared `rpc::handle_rpc_with_db`, and writes responses to stdout.
//! This is the transport used by `heliosdb-nano mcp-server` and by any
//! MCP client configured via
//! `{"mcpServers":{"heliosdb":{"command":"heliosdb-nano","args":["mcp-server"]}}}`.
//!
//! Supports `notifications/progress`: when a `tools/call` request
//! includes `_meta.progressToken`, the handler runs on a blocking
//! thread and progress events are forwarded as JSON-RPC notifications
//! interleaved with stdout output, ahead of the final response.
//!
//! HTTP / WebSocket / SSE transports are implemented separately but
//! share the same `rpc::handle_rpc_with_db` core so tool / resource
//! behaviour is identical across wire formats.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::EmbeddedDatabase;

use super::rpc::{handle_rpc_with_db, RpcRequest};
use super::streaming::call_tool_streaming;
use super::tools::ToolOutcome;

/// MCP stdio server bound to a specific `EmbeddedDatabase`.
pub struct McpServer {
    db: Arc<EmbeddedDatabase>,
}

impl McpServer {
    pub fn new(db: Arc<EmbeddedDatabase>) -> Self {
        Self { db }
    }

    /// Drive the stdio loop until EOF.
    pub async fn run(&mut self) -> crate::Result<()> {
        info!("starting HeliosDB MCP server (stdio)");
        let stdin = std::io::stdin();
        let stdout = Arc::new(Mutex::new(std::io::stdout()));
        let reader = BufReader::new(stdin.lock());

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    error!("stdin read failed: {e}");
                    continue;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            debug!("<< {line}");

            let req: RpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    let err = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": serde_json::Value::Null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    });
                    write_line(&stdout, &err.to_string()).await;
                    continue;
                }
            };

            if req.method == "initialized" {
                continue;
            }

            // Streaming path: tools/call with _meta.progressToken
            // emits notifications/progress lines ahead of the final
            // tools/call response.
            if req.method == "tools/call" {
                if let Some(token) = extract_progress_token(&req.params) {
                    self.dispatch_streaming_tools_call(&stdout, req, token).await;
                    continue;
                }
            }

            let resp = handle_rpc_with_db(self.db.as_ref(), req);
            let json = match serde_json::to_string(&resp) {
                Ok(j) => j,
                Err(e) => {
                    error!("response serialize failed: {e}");
                    continue;
                }
            };
            debug!(">> {json}");
            write_line(&stdout, &json).await;
        }

        info!("MCP server shutting down");
        Ok(())
    }

    async fn dispatch_streaming_tools_call(
        &self,
        stdout: &Arc<Mutex<std::io::Stdout>>,
        req: RpcRequest,
        progress_token: serde_json::Value,
    ) {
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
            let err = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": "tools/call requires 'name'" }
            });
            write_line(stdout, &err.to_string()).await;
            return;
        };

        let (mut rx, handle) = call_tool_streaming(Some(self.db.clone()), name, args);
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
            let notif = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": params,
            });
            write_line(stdout, &notif.to_string()).await;
        }

        let outcome = handle
            .await
            .unwrap_or_else(|e| ToolOutcome::err(format!("tool task panicked: {e}")));
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
        write_line(stdout, &resp.to_string()).await;
    }
}

fn extract_progress_token(params: &serde_json::Value) -> Option<serde_json::Value> {
    let token = params.get("_meta")?.get("progressToken")?;
    if token.is_string() || token.is_number() {
        Some(token.clone())
    } else {
        None
    }
}

async fn write_line(stdout: &Arc<Mutex<std::io::Stdout>>, line: &str) {
    let mut out = stdout.lock().await;
    if let Err(e) = writeln!(out, "{line}") {
        error!("stdout write failed: {e}");
    }
    if let Err(e) = out.flush() {
        error!("stdout flush failed: {e}");
    }
}
