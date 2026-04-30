//! JSON-RPC 2.0 dispatcher for the MCP endpoint.
//!
//! The MCP protocol is JSON-RPC 2.0 over a fixed method namespace
//! (`initialize`, `tools/list`, `tools/call`, `resources/list`,
//! `resources/read`, `ping`). This module is transport-agnostic — the
//! stdio server (`server.rs`) and any HTTP / WebSocket / SSE mount
//! share the same dispatch entry point.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};

use crate::EmbeddedDatabase;

use super::resources::{list_resources, read_resource};
use super::tools::{call_tool, list_tools, ToolDescriptor};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<JsonValue>,
    pub method: String,
    #[serde(default)]
    pub params: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: JsonValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

impl RpcResponse {
    pub fn success(id: JsonValue, result: JsonValue) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }
    pub fn error(id: JsonValue, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError { code, message: message.into(), data: None }),
        }
    }
}

/// In-process-only dispatch — DB-backed tools that don't have an
/// `EmbeddedDatabase` available will return `isError: true` via the
/// tools layer. Useful for transports that don't thread DB state
/// (e.g. the smoke test for a freshly-mounted HTTP handler).
pub fn handle_rpc(req: RpcRequest) -> RpcResponse {
    handle_rpc_opt(None, req)
}

/// Full dispatch with a live database. Use this from the stdio server
/// and from any HTTP handler that has access to an `EmbeddedDatabase`.
pub fn handle_rpc_with_db(db: &EmbeddedDatabase, req: RpcRequest) -> RpcResponse {
    handle_rpc_opt(Some(db), req)
}

fn handle_rpc_opt(db: Option<&EmbeddedDatabase>, req: RpcRequest) -> RpcResponse {
    let id = req.id.clone().unwrap_or(JsonValue::Null);
    match req.method.as_str() {
        "initialize" => RpcResponse::success(id, initialize_result()),
        "tools/list" => {
            let verbose = req
                .params
                .get("verbose")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            RpcResponse::success(id, tools_list_result(verbose))
        }
        "tools/call" => match tools_call(db, &req.params) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, -32000, e),
        },
        "resources/list" => RpcResponse::success(id, resources_list_result()),
        "resources/read" => match db {
            Some(d) => match resources_read(d, &req.params) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, -32000, e),
            },
            None => RpcResponse::error(
                id,
                -32000,
                "resources/read requires a database connection",
            ),
        },
        "ping" => RpcResponse::success(id, json!({})),
        // Nano-specific introspection RPC mirroring `GET /mcp/info`.
        // Returns the same payload the HTTP info endpoint emits so
        // stdio / WebSocket clients can pull a single discovery
        // packet without juggling resources/list + tools/list +
        // initialize.
        "helios/info" => RpcResponse::success(id, info_result()),
        other => RpcResponse::error(id, -32601, format!("Method not found: {other}")),
    }
}

/// Single-shot discovery payload — server info, advertised
/// capabilities, full tool catalogue (verbose), and resource list.
pub(crate) fn info_result() -> JsonValue {
    let tools: Vec<JsonValue> = list_tools()
        .into_iter()
        .map(|t| tool_to_json(t, true))
        .collect();
    let resources: Vec<JsonValue> = super::resources::list_resources()
        .into_iter()
        .map(|(uri, name, desc)| {
            json!({
                "uri": uri,
                "name": name,
                "description": desc,
                "mimeType": "application/json",
            })
        })
        .collect();
    let cache = super::result_cache::stats();
    let cache_hit_rate = {
        let total = cache.hits + cache.misses;
        if total == 0 { 0.0 } else { cache.hits as f64 / total as f64 }
    };
    json!({
        "serverInfo": {
            "name": "heliosdb-nano",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools":     { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
        },
        "tools": tools,
        "resources": resources,
        "tool_count": tools.len(),
        // Server-side LRU cache stats (read-only `tools/call`
        // results). Plugin / ops surface this in `status` for hit
        // rate, generation drift after writes, and capacity vs len.
        "cache": {
            "size":       cache.size,
            "capacity":   cache.capacity,
            "generation": cache.generation,
            "hits":       cache.hits,
            "misses":     cache.misses,
            "evictions":  cache.evictions,
            "hit_rate":   cache_hit_rate,
        },
    })
}

fn initialize_result() -> JsonValue {
    json!({
        "protocolVersion": "2024-11-05",
        "serverInfo": {
            "name": "heliosdb-nano",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "tools":     { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
        }
    })
}

fn tools_list_result(verbose: bool) -> JsonValue {
    let tools: Vec<JsonValue> = list_tools()
        .into_iter()
        .map(|t| tool_to_json(t, verbose))
        .collect();
    json!({ "tools": tools })
}

fn tool_to_json(t: ToolDescriptor, verbose: bool) -> JsonValue {
    let mut out = json!({
        "name": t.name,
        "description": t.description,
        "inputSchema": t.input_schema,
    });
    if verbose {
        // `category` distinguishes the unified DB-backed catalogue
        // (heliosdb_*) from the auto-registered helios_* extensions
        // declared via mcp_tool!.  Useful for clients wanting to
        // gate which tools they expose to a model.
        let category = if t.name.starts_with("helios_") {
            "extension"
        } else {
            "core"
        };
        let needs_db = match t.name {
            "heliosdb_bm25_index"
            | "heliosdb_hybrid_search"
            | "heliosdb_graph_add_edge"
            | "heliosdb_graph_traverse"
            | "heliosdb_graph_path"
            | "heliosdb_embed_and_store" => false,
            _ => true,
        };
        if let Some(obj) = out.as_object_mut() {
            obj.insert("category".into(), json!(category));
            obj.insert("requiresDatabase".into(), json!(needs_db));
        }
    }
    out
}

fn tools_call(db: Option<&EmbeddedDatabase>, params: &JsonValue) -> Result<JsonValue, String> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "tools/call requires 'name'".to_string())?;
    let args = params.get("arguments").cloned().unwrap_or(JsonValue::Null);
    let outcome = call_tool(db, name, args);
    Ok(json!({
        "isError": outcome.is_error,
        "content": [
            { "type": "text", "text": outcome.payload.to_string() }
        ]
    }))
}

fn resources_list_result() -> JsonValue {
    let entries: Vec<JsonValue> = list_resources()
        .into_iter()
        .map(|(uri, name, desc)| {
            json!({
                "uri": uri,
                "name": name,
                "description": desc,
                "mimeType": "application/json",
            })
        })
        .collect();
    json!({ "resources": entries })
}

fn resources_read(db: &EmbeddedDatabase, params: &JsonValue) -> Result<JsonValue, String> {
    let uri = params
        .get("uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "resources/read requires 'uri'".to_string())?;
    match read_resource(db, uri) {
        Some(Ok(payload)) => Ok(json!({
            "contents": [{
                "uri": payload.uri,
                "mimeType": payload.mime_type,
                "text": payload.text,
            }]
        })),
        Some(Err(e)) => Err(e),
        None => Err(format!("unknown resource: {uri}")),
    }
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
            params: JsonValue::Null,
        };
        let resp = handle_rpc(req);
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "heliosdb-nano");
    }

    #[test]
    fn tools_list_non_empty() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "tools/list".into(),
            params: JsonValue::Null,
        };
        let resp = handle_rpc(req);
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        assert!(tools.len() >= 16, "expected 10 DB + 6 in-process tools, got {}", tools.len());
    }

    #[test]
    fn unknown_method_returns_32601() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(3)),
            method: "does/not/exist".into(),
            params: JsonValue::Null,
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

    #[test]
    fn tools_call_db_tool_without_db_is_isError() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(5)),
            method: "tools/call".into(),
            params: json!({ "name": "heliosdb_query", "arguments": { "sql": "SELECT 1" } }),
        };
        let resp = handle_rpc(req);
        let result = resp.result.expect("result");
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn tools_call_db_tool_with_db_succeeds() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT4)").unwrap();
        db.execute("INSERT INTO t VALUES (42)").unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(6)),
            method: "tools/call".into(),
            params: json!({ "name": "heliosdb_query", "arguments": { "sql": "SELECT id FROM t" } }),
        };
        let resp = handle_rpc_with_db(&db, req);
        let result = resp.result.expect("result");
        assert_eq!(result["isError"], false);
    }

    #[test]
    fn resources_read_requires_db() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(7)),
            method: "resources/read".into(),
            params: json!({ "uri": "heliosdb://schema" }),
        };
        let resp = handle_rpc(req);
        assert!(resp.error.is_some());
    }

    #[test]
    fn resources_read_with_db_succeeds() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT4)").unwrap();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(8)),
            method: "resources/read".into(),
            params: json!({ "uri": "heliosdb://schema" }),
        };
        let resp = handle_rpc_with_db(&db, req);
        let result = resp.result.expect("result");
        assert!(result["contents"][0]["text"].as_str().unwrap().contains("tables"));
    }
}
