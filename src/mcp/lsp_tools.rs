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

use crate::code_graph::{
    self, ast_diff, lsp_body_diff, lsp_references_diff, AsOfRef, CallDirection, DefinitionHint,
};
use crate::{EmbeddedDatabase, Value};

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

// ---- helios_lsp_document_symbols -------------------------------------

#[derive(Debug, Deserialize)]
struct DocSymArgs {
    /// File path as it appears in `_hdb_code_files.path`.
    path: String,
    /// Optional kind filter (e.g. ["function", "struct"]).
    #[serde(default)]
    kinds: Vec<String>,
}

fn lsp_document_symbols_handler(
    db: Option<&EmbeddedDatabase>,
    args: JsonValue,
) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_lsp_document_symbols requires a database connection");
    };
    let input: DocSymArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let mut sql = String::from(
        "SELECT s.node_id, s.qualified, s.name, s.kind, s.signature, \
                s.line_start, s.line_end \
         FROM _hdb_code_symbols s \
         JOIN _hdb_code_files f ON f.node_id = s.file_id \
         WHERE f.path = $1",
    );
    if !input.kinds.is_empty() {
        sql.push_str(" AND s.kind IN (");
        for (i, _) in input.kinds.iter().enumerate() {
            if i > 0 { sql.push(','); }
            sql.push_str(&format!("${}", i + 2));
        }
        sql.push(')');
    }
    sql.push_str(" ORDER BY s.line_start, s.node_id");

    let mut params: Vec<Value> = vec![Value::String(input.path.clone())];
    for k in &input.kinds {
        params.push(Value::String(k.clone()));
    }
    let rows = match db.query_params(&sql, &params) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::err(format!("document_symbols query failed: {e}")),
    };
    let symbols: Vec<JsonValue> = rows
        .iter()
        .map(|r| {
            json!({
                "symbol_id": value_to_i64(r.values.first()),
                "qualified": value_to_string(r.values.get(1)),
                "name":      value_to_string(r.values.get(2)),
                "kind":      value_to_string(r.values.get(3)),
                "signature": value_to_string(r.values.get(4)),
                "line_start": value_to_i64(r.values.get(5)),
                "line_end":   value_to_i64(r.values.get(6)),
            })
        })
        .collect();
    ToolOutcome::ok(json!({
        "path": input.path,
        "count": symbols.len(),
        "symbols": symbols,
    }))
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_lsp_document_symbols",
        description: "File outline — list every symbol in `_hdb_code_files.path`, ordered by line.",
        schema: lsp_document_symbols_schema,
        handler: lsp_document_symbols_handler,
    }
}

fn lsp_document_symbols_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "path":  { "type": "string", "description": "File path as stored in _hdb_code_files.path" },
            "kinds": { "type": "array",  "items": { "type": "string" }, "description": "Optional kind filter" }
        },
        "required": ["path"]
    })
}

// ---- helios_lsp_rename_preview ---------------------------------------

#[derive(Debug, Deserialize)]
struct RenameArgs {
    symbol_id: i64,
    new_name: String,
}

fn lsp_rename_preview_handler(
    db: Option<&EmbeddedDatabase>,
    args: JsonValue,
) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_lsp_rename_preview requires a database connection");
    };
    let input: RenameArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    if input.new_name.trim().is_empty() {
        return ToolOutcome::err("new_name must not be empty");
    }
    // Pull definition site for the requested symbol_id.
    let def_rows = match db.query_params(
        "SELECT s.qualified, s.name, s.kind, f.path, s.line_start, s.signature \
         FROM _hdb_code_symbols s \
         JOIN _hdb_code_files f ON f.node_id = s.file_id \
         WHERE s.node_id = $1",
        &[Value::Int8(input.symbol_id)],
    ) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::err(format!("rename_preview lookup failed: {e}")),
    };
    let Some(def) = def_rows.first() else {
        return ToolOutcome::ok(json!({
            "symbol_id": input.symbol_id,
            "new_name": input.new_name,
            "found": false,
            "edits": [],
        }));
    };

    let qualified = value_to_string(def.values.first()).unwrap_or_default();
    let old_name = value_to_string(def.values.get(1)).unwrap_or_default();
    let kind = value_to_string(def.values.get(2)).unwrap_or_default();
    let def_path = value_to_string(def.values.get(3)).unwrap_or_default();
    let def_line = value_to_i64(def.values.get(4)).unwrap_or(0);
    let signature = value_to_string(def.values.get(5)).unwrap_or_default();

    // Pull every reference site that points at this symbol.
    let ref_rows = match code_graph::lsp_references(db, input.symbol_id) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::err(format!("rename_preview refs failed: {e}")),
    };

    // Build an edit list. Phase-1 preview is line-granular: each
    // reference site is one edit at (path, line) replacing `old_name`
    // with `new_name`. Caller is responsible for actually rewriting.
    let mut edits: Vec<JsonValue> = Vec::new();
    edits.push(json!({
        "kind": "definition",
        "path": def_path,
        "line": def_line,
        "old_name": old_name,
        "new_name": input.new_name,
        "signature": signature,
    }));
    for r in &ref_rows {
        edits.push(json!({
            "kind": "reference",
            "path": r.path,
            "line": r.line,
            "ref_kind": r.kind,
            "old_name": old_name,
            "new_name": input.new_name,
            "caller_symbol_id": r.caller_symbol_id,
        }));
    }

    ToolOutcome::ok(json!({
        "symbol_id": input.symbol_id,
        "qualified": qualified,
        "kind": kind,
        "old_name": old_name,
        "new_name": input.new_name,
        "found": true,
        "edit_count": edits.len(),
        "edits": edits,
        "applied": false,
        "note": "preview-only: nothing written. Apply manually or via a follow-up tool.",
    }))
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_lsp_rename_preview",
        description: "Preview a symbol rename — collects definition + every reference site \
                      and returns the edit list. Read-only; no source rewrite.",
        schema: lsp_rename_preview_schema,
        handler: lsp_rename_preview_handler,
    }
}

fn lsp_rename_preview_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "symbol_id": { "type": "integer" },
            "new_name":  { "type": "string" }
        },
        "required": ["symbol_id", "new_name"]
    })
}

// ---- Diff tools (FR 3.3 wrappers) ------------------------------------

fn parse_as_of(v: &JsonValue) -> Result<AsOfRef, String> {
    if v.is_null() {
        return Ok(AsOfRef::Now);
    }
    if let Some(s) = v.as_str() {
        if s.eq_ignore_ascii_case("now") {
            return Ok(AsOfRef::Now);
        }
        return Err(format!(
            "as_of must be an object {{ commit | timestamp | now }}, got string '{s}'"
        ));
    }
    let obj = v.as_object().ok_or_else(|| {
        "as_of must be an object with one of: commit, timestamp, now=true".to_string()
    })?;
    if obj.get("now").and_then(|x| x.as_bool()) == Some(true) {
        return Ok(AsOfRef::Now);
    }
    if let Some(c) = obj.get("commit").and_then(|x| x.as_str()) {
        return Ok(AsOfRef::Commit(c.to_string()));
    }
    if let Some(t) = obj.get("timestamp").and_then(|x| x.as_str()) {
        return Ok(AsOfRef::Timestamp(t.to_string()));
    }
    Err("as_of must specify one of: commit, timestamp, now".to_string())
}

#[derive(Debug, Deserialize)]
struct RefDiffArgs {
    symbol_id: i64,
    #[serde(default)]
    at_a: JsonValue,
    #[serde(default)]
    at_b: JsonValue,
}

fn lsp_references_diff_handler(
    db: Option<&EmbeddedDatabase>,
    args: JsonValue,
) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_lsp_references_diff requires a database connection");
    };
    let input: RefDiffArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let a = match parse_as_of(&input.at_a) { Ok(x) => x, Err(e) => return ToolOutcome::err(e) };
    let b = match parse_as_of(&input.at_b) { Ok(x) => x, Err(e) => return ToolOutcome::err(e) };
    match lsp_references_diff(db, input.symbol_id, &a, &b) {
        Ok(rows) => ToolOutcome::ok(json!({
            "symbol_id": input.symbol_id,
            "count": rows.len(),
            "rows": rows
                .iter()
                .map(|r| json!({
                    "change": r.change.as_str(),
                    "path": r.path,
                    "line": r.line,
                    "caller_symbol_id": r.caller_symbol_id,
                }))
                .collect::<Vec<_>>(),
        })),
        Err(e) => ToolOutcome::err(format!("lsp_references_diff failed: {e}")),
    }
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_lsp_references_diff",
        description: "Diff a symbol's reference set across two AS OF points.",
        schema: lsp_references_diff_schema,
        handler: lsp_references_diff_handler,
    }
}

fn as_of_schema_fragment() -> JsonValue {
    json!({
        "oneOf": [
            { "type": "string", "enum": ["now"] },
            { "type": "object", "properties": { "now":       { "type": "boolean" } }, "required": ["now"] },
            { "type": "object", "properties": { "commit":    { "type": "string"  } }, "required": ["commit"] },
            { "type": "object", "properties": { "timestamp": { "type": "string"  } }, "required": ["timestamp"] }
        ]
    })
}

fn lsp_references_diff_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "symbol_id": { "type": "integer" },
            "at_a":      as_of_schema_fragment(),
            "at_b":      as_of_schema_fragment()
        },
        "required": ["symbol_id"]
    })
}

#[derive(Debug, Deserialize)]
struct BodyDiffArgs {
    symbol_id: i64,
    #[serde(default)]
    at_a: JsonValue,
    #[serde(default)]
    at_b: JsonValue,
}

fn lsp_body_diff_handler(db: Option<&EmbeddedDatabase>, args: JsonValue) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_lsp_body_diff requires a database connection");
    };
    let input: BodyDiffArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let a = match parse_as_of(&input.at_a) { Ok(x) => x, Err(e) => return ToolOutcome::err(e) };
    let b = match parse_as_of(&input.at_b) { Ok(x) => x, Err(e) => return ToolOutcome::err(e) };
    match lsp_body_diff(db, input.symbol_id, &a, &b) {
        Ok(rows) => ToolOutcome::ok(json!({
            "symbol_id": input.symbol_id,
            "count": rows.len(),
            "lines": rows
                .iter()
                .map(|r| json!({
                    "line_a": r.line_a,
                    "line_b": r.line_b,
                    "op": r.op.as_str(),
                    "text": r.text,
                }))
                .collect::<Vec<_>>(),
        })),
        Err(e) => ToolOutcome::err(format!("lsp_body_diff failed: {e}")),
    }
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_lsp_body_diff",
        description: "Myers-LCS line-level diff of a symbol's signature across two AS OF points.",
        schema: lsp_body_diff_schema,
        handler: lsp_body_diff_handler,
    }
}

fn lsp_body_diff_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "symbol_id": { "type": "integer" },
            "at_a":      as_of_schema_fragment(),
            "at_b":      as_of_schema_fragment()
        },
        "required": ["symbol_id"]
    })
}

#[derive(Debug, Deserialize)]
struct AstDiffArgs {
    path: String,
    #[serde(default)]
    at_a: JsonValue,
    #[serde(default)]
    at_b: JsonValue,
}

fn ast_diff_handler(db: Option<&EmbeddedDatabase>, args: JsonValue) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_ast_diff requires a database connection");
    };
    let input: AstDiffArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let a = match parse_as_of(&input.at_a) { Ok(x) => x, Err(e) => return ToolOutcome::err(e) };
    let b = match parse_as_of(&input.at_b) { Ok(x) => x, Err(e) => return ToolOutcome::err(e) };
    match ast_diff(db, &input.path, &a, &b) {
        Ok(rows) => ToolOutcome::ok(json!({
            "path": input.path,
            "count": rows.len(),
            "rows": rows
                .iter()
                .map(|r| json!({
                    "change": r.change.as_str(),
                    "kind": r.kind,
                    "qualified": r.qualified,
                    "line_a": r.line_a,
                    "line_b": r.line_b,
                }))
                .collect::<Vec<_>>(),
        })),
        Err(e) => ToolOutcome::err(format!("ast_diff failed: {e}")),
    }
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_ast_diff",
        description: "File-level structural diff (added/removed/moved symbols) across two AS OF points.",
        schema: ast_diff_schema,
        handler: ast_diff_handler,
    }
}

fn ast_diff_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "at_a": as_of_schema_fragment(),
            "at_b": as_of_schema_fragment()
        },
        "required": ["path"]
    })
}

// ---- Helpers ----------------------------------------------------------

fn value_to_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn value_to_i64(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int2(n)) => Some(i64::from(*n)),
        Some(Value::Int4(n)) => Some(i64::from(*n)),
        Some(Value::Int8(n)) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::auto_register::registered;

    #[test]
    fn all_lsp_tools_are_registered() {
        let names: Vec<_> = registered().map(|t| t.name).collect();
        for n in [
            "helios_lsp_definition",
            "helios_lsp_references",
            "helios_lsp_call_hierarchy",
            "helios_lsp_hover",
            "helios_lsp_document_symbols",
            "helios_lsp_rename_preview",
            "helios_lsp_references_diff",
            "helios_lsp_body_diff",
            "helios_ast_diff",
        ] {
            assert!(names.contains(&n), "missing {n} in {names:?}");
        }
    }
}
