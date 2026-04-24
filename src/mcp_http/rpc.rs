//! JSON-RPC 2.0 framing for the MCP endpoint.
//!
//! The MCP protocol is JSON-RPC 2.0 with a fixed method namespace
//! (`initialize`, `tools/list`, `tools/call`, `resources/list`,
//! `resources/read`, `ping`). Phase-4 MVP implements the core three
//! (`initialize`, `tools/list`, `tools/call`); unknown methods return
//! the standard JSON-RPC error `-32601 Method not found`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mcp_extensions::tools::{call_tool, list_tools, ToolDescriptor};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Dispatch a single JSON-RPC request. Pure function — no Axum / HTTP
/// dependencies so the same handler can be reused from other
/// transports later.
pub fn handle_rpc(req: RpcRequest) -> RpcResponse {
    let id = req.id.clone().unwrap_or(Value::Null);
    match req.method.as_str() {
        "initialize" => RpcResponse::success(id, initialize_result()),
        "tools/list" => RpcResponse::success(id, tools_list_result()),
        "tools/call" => match tools_call(&req.params) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, -32000, e),
        },
        "ping" => RpcResponse::success(id, json!({})),
        other => RpcResponse::error(id, -32601, format!("Method not found: {other}")),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "serverInfo": {
            "name": "heliosdb-nano",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
        }
    })
}

fn tools_list_result() -> Value {
    let tools: Vec<Value> = list_tools().into_iter().map(tool_to_json).collect();
    json!({ "tools": tools })
}

fn tool_to_json(t: ToolDescriptor) -> Value {
    json!({
        "name": t.name,
        "description": t.description,
        "inputSchema": t.input_schema,
    })
}

fn tools_call(params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "tools/call requires 'name'".to_string())?;
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let outcome = call_tool(name, args);
    Ok(json!({
        "isError": outcome.is_error,
        "content": [
            {
                "type": "text",
                "text": outcome.payload.to_string(),
            }
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_returns_server_info() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "initialize".into(),
            params: Value::Null,
        };
        let resp = handle_rpc(req);
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "heliosdb-nano");
        assert!(result["protocolVersion"].as_str().is_some());
    }

    #[test]
    fn tools_list_non_empty() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "tools/list".into(),
            params: Value::Null,
        };
        let resp = handle_rpc(req);
        assert!(resp.error.is_none());
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        assert!(!tools.is_empty(), "tools/list should return at least one tool");
    }

    #[test]
    fn unknown_method_returns_32601() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(3)),
            method: "does/not/exist".into(),
            params: Value::Null,
        };
        let resp = handle_rpc(req);
        let err = resp.error.expect("expected error");
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn tools_call_without_name_errors_32000() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(4)),
            method: "tools/call".into(),
            params: json!({}),
        };
        let resp = handle_rpc(req);
        let err = resp.error.expect("expected error");
        assert_eq!(err.code, -32000);
    }
}
