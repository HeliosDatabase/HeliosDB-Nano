//! Stdio MCP server.
//!
//! Reads JSON-RPC 2.0 requests line-by-line from stdin, dispatches via
//! the shared `rpc::handle_rpc_with_db`, and writes responses to stdout.
//! This is the transport used by `heliosdb-nano mcp-server` and by any
//! MCP client configured via
//! `{"mcpServers":{"heliosdb":{"command":"heliosdb-nano","args":["mcp-server"]}}}`.
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
                    // JSON-RPC spec: parse errors use id = null.
                    let err = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": serde_json::Value::Null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    });
                    let mut out = stdout.lock().await;
                    let _ = writeln!(out, "{err}");
                    let _ = out.flush();
                    continue;
                }
            };

            // MCP notification: client tells server it's ready. No
            // response expected.
            if req.method == "initialized" {
                continue;
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
            let mut out = stdout.lock().await;
            if let Err(e) = writeln!(out, "{json}") {
                error!("stdout write failed: {e}");
            }
            if let Err(e) = out.flush() {
                error!("stdout flush failed: {e}");
            }
        }

        info!("MCP server shutting down");
        Ok(())
    }
}
