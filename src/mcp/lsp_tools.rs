//! Auto-registered MCP tools that surface the `code_graph::lsp_*`
//! functions over the JSON-RPC dispatcher.
//!
//! Loading this module is purely a side effect: each `mcp_tool!` call
//! submits an [`auto_register::McpExtensionTool`] entry to the
//! process-wide inventory at startup, which `tools::list_tools` and
//! `tools::call_tool` consult automatically.
//!
//! Adding a new LSP-shaped tool is a one-block change in this file —
//! no edits to `tools.rs` required. That's the FR 5 promise.

use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::code_graph::{self, CallDirection, DefinitionHint};
use crate::EmbeddedDatabase;

use super::auto_register::McpExtensionTool;
use super::tools::ToolOutcome;

// ---- helios_lsp_definition -------------------------------------------

#[derive(Debug, Deserialize)]
struct DefArgs {
    name: String,
    #[serde(default)]
    hint_file: Option<String>,
    #[serde(default)]
    hint_kind: Option<String>,
}

fn lsp_definition_handler(
    db: Option<&EmbeddedDatabase>,
    args: JsonValue,
) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_lsp_definition requires a database connection");
    };
    let input: DefArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let hint = DefinitionHint {
        hint_file: input.hint_file,
        hint_kind: input.hint_kind,
    };
    match code_graph::lsp_definition(db, &input.name, &hint) {
        Ok(rows) => ToolOutcome::ok(json!({
            "name": input.name,
            "count": rows.len(),
            "rows": rows
                .iter()
                .map(|r| json!({
                    "symbol_id": r.symbol_id,
                    "path": r.path,
                    "line": r.line,
                    "signature": r.signature,
                    "qualified": r.qualified,
                    "score": r.score,
                }))
                .collect::<Vec<_>>(),
        })),
        Err(e) => ToolOutcome::err(format!("lsp_definition failed: {e}")),
    }
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_lsp_definition",
        description: "Locate where a symbol is defined, with optional file/kind hints.",
        schema: lsp_definition_schema,
        handler: lsp_definition_handler,
    }
}

fn lsp_definition_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "name":      { "type": "string", "description": "Symbol name to resolve" },
            "hint_file": { "type": "string", "description": "Optional path filter" },
            "hint_kind": { "type": "string", "description": "Optional kind filter (function / struct / …)" }
        },
        "required": ["name"]
    })
}

// ---- helios_lsp_references -------------------------------------------

#[derive(Debug, Deserialize)]
struct RefsArgs {
    symbol_id: i64,
}

fn lsp_references_handler(
    db: Option<&EmbeddedDatabase>,
    args: JsonValue,
) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_lsp_references requires a database connection");
    };
    let input: RefsArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    match code_graph::lsp_references(db, input.symbol_id) {
        Ok(rows) => ToolOutcome::ok(json!({
            "symbol_id": input.symbol_id,
            "count": rows.len(),
            "rows": rows
                .iter()
                .map(|r| json!({
                    "file_id": r.file_id,
                    "path": r.path,
                    "line": r.line,
                    "kind": r.kind,
                    "caller_symbol_id": r.caller_symbol_id,
                }))
                .collect::<Vec<_>>(),
        })),
        Err(e) => ToolOutcome::err(format!("lsp_references failed: {e}")),
    }
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_lsp_references",
        description: "List every call/use site of the given symbol.",
        schema: lsp_references_schema,
        handler: lsp_references_handler,
    }
}

fn lsp_references_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "symbol_id": { "type": "integer", "description": "Target symbol's node_id" }
        },
        "required": ["symbol_id"]
    })
}

// ---- helios_lsp_call_hierarchy ---------------------------------------

#[derive(Debug, Deserialize)]
struct CallArgs {
    symbol_id: i64,
    #[serde(default = "default_direction")]
    direction: String,
    #[serde(default = "default_depth")]
    depth: u32,
}

fn default_direction() -> String { "outgoing".to_string() }
fn default_depth() -> u32 { 3 }

fn lsp_call_hierarchy_handler(
    db: Option<&EmbeddedDatabase>,
    args: JsonValue,
) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_lsp_call_hierarchy requires a database connection");
    };
    let input: CallArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let direction = match input.direction.to_ascii_lowercase().as_str() {
        "incoming" | "callers" | "in" => CallDirection::Incoming,
        "outgoing" | "callees" | "out" => CallDirection::Outgoing,
        other => return ToolOutcome::err(format!("unknown direction '{other}'")),
    };
    match code_graph::lsp_call_hierarchy(db, input.symbol_id, direction, input.depth) {
        Ok(rows) => ToolOutcome::ok(json!({
            "symbol_id": input.symbol_id,
            "direction": format!("{direction:?}"),
            "depth": input.depth,
            "count": rows.len(),
            "rows": rows
                .iter()
                .map(|r| json!({
                    "depth": r.depth,
                    "symbol_id": r.symbol_id,
                    "qualified": r.qualified,
                    "path": r.path,
                    "line": r.line,
                }))
                .collect::<Vec<_>>(),
        })),
        Err(e) => ToolOutcome::err(format!("lsp_call_hierarchy failed: {e}")),
    }
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_lsp_call_hierarchy",
        description: "BFS-style call tree rooted at a symbol (incoming or outgoing).",
        schema: lsp_call_hierarchy_schema,
        handler: lsp_call_hierarchy_handler,
    }
}

fn lsp_call_hierarchy_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "symbol_id": { "type": "integer" },
            "direction": { "type": "string", "enum": ["incoming", "outgoing"], "default": "outgoing" },
            "depth":     { "type": "integer", "default": 3 }
        },
        "required": ["symbol_id"]
    })
}

// ---- helios_lsp_hover ------------------------------------------------

#[derive(Debug, Deserialize)]
struct HoverArgs {
    symbol_id: i64,
}

fn lsp_hover_handler(db: Option<&EmbeddedDatabase>, args: JsonValue) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_lsp_hover requires a database connection");
    };
    let input: HoverArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    match code_graph::lsp_hover(db, input.symbol_id) {
        Ok(Some(h)) => ToolOutcome::ok(json!({
            "symbol_id": input.symbol_id,
            "signature": h.signature,
            "doc": h.doc,
            "ai_summary": h.ai_summary,
        })),
        Ok(None) => ToolOutcome::ok(json!({
            "symbol_id": input.symbol_id,
            "found": false,
        })),
        Err(e) => ToolOutcome::err(format!("lsp_hover failed: {e}")),
    }
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_lsp_hover",
        description: "Return the signature / doc / AI summary for a symbol.",
        schema: lsp_hover_schema,
        handler: lsp_hover_handler,
    }
}

fn lsp_hover_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "symbol_id": { "type": "integer" }
        },
        "required": ["symbol_id"]
    })
}

#[cfg(test)]
mod tests {
    use super::super::auto_register::registered;

    #[test]
    fn all_four_lsp_tools_are_registered() {
        let names: Vec<_> = registered().map(|t| t.name).collect();
        for n in [
            "helios_lsp_definition",
            "helios_lsp_references",
            "helios_lsp_call_hierarchy",
            "helios_lsp_hover",
        ] {
            assert!(names.contains(&n), "missing {n} in {names:?}");
        }
    }
}
