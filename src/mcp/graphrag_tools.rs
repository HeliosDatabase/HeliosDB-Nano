//! Auto-registered MCP tool wrapping `graph_rag::graph_rag_search`.
//!
//! Closes the gap between the embedded Rust API for cross-modal seed
//! → expand → return queries and the MCP transport surface — without
//! it, the flagship FR 4 query has to be invoked over SQL rather
//! than over JSON-RPC, defeating the "zero-middleware for AI agents"
//! pitch.
//!
//! Registers on every `mcp-endpoint + graph-rag` build via
//! `inventory::submit!`.

use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::graph_rag::{self, Direction, GraphRagOptions};
use crate::EmbeddedDatabase;

use super::auto_register::McpExtensionTool;
use super::progress::{self, ProgressEvent};
use super::tools::ToolOutcome;

#[derive(Debug, Deserialize)]
struct SearchArgs {
    seed_text: String,
    #[serde(default)]
    seed_kinds: Vec<String>,
    #[serde(default = "default_hops")]
    hops: u32,
    #[serde(default)]
    edge_kinds: Vec<String>,
    #[serde(default = "default_direction")]
    direction: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_hops() -> u32 { 2 }
fn default_limit() -> usize { 50 }
fn default_direction() -> String { "both".to_string() }

fn parse_direction(s: &str) -> Result<Direction, String> {
    match s.to_ascii_lowercase().as_str() {
        "out" | "outgoing" | "->"  => Ok(Direction::Out),
        "in"  | "incoming" | "<-"  => Ok(Direction::In),
        "both" | "bi" | "<->"      => Ok(Direction::Both),
        other => Err(format!("unknown direction '{other}' (expected out / in / both)")),
    }
}

fn graphrag_search_handler(
    db: Option<&EmbeddedDatabase>,
    args: JsonValue,
) -> ToolOutcome {
    let Some(db) = db else {
        return ToolOutcome::err("helios_graphrag_search requires a database connection");
    };
    let input: SearchArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let direction = match parse_direction(&input.direction) {
        Ok(d) => d,
        Err(e) => return ToolOutcome::err(e),
    };
    let opts = GraphRagOptions {
        seed_text: input.seed_text,
        seed_kinds: input.seed_kinds,
        hops: input.hops,
        edge_kinds: input.edge_kinds,
        direction,
        limit: input.limit,
    };

    // Bookend the search with two progress events. We can't emit
    // mid-flight events without changing graph_rag_search's signature
    // — but seeds-found / final-count is enough signal for an agent
    // to render a "thinking" indicator vs a stuck call.
    progress::emit(
        ProgressEvent::new(0.0)
            .with_total(opts.limit as f64)
            .with_message(format!(
                "graph_rag_search: seeding for '{}', hops={}",
                opts.seed_text, opts.hops
            )),
    );

    match graph_rag::graph_rag_search(db, &opts) {
        Ok(rows) => {
            progress::emit(
                ProgressEvent::new(rows.len() as f64)
                    .with_total(opts.limit as f64)
                    .with_message(format!("graph_rag_search: {} hits", rows.len())),
            );
            ToolOutcome::ok(json!({
                "seed_text": opts.seed_text,
                "hops": opts.hops,
                "direction": format!("{:?}", opts.direction),
                "count": rows.len(),
                "rows": rows
                    .iter()
                    .map(|r| json!({
                        "node_id": r.node_id,
                        "node_kind": r.node_kind,
                        "title": r.title,
                        "text": r.text,
                        "source_ref": r.source_ref,
                        "hop_distance": r.hop_distance,
                    }))
                    .collect::<Vec<_>>(),
            }))
        }
        Err(e) => ToolOutcome::err(format!("graph_rag_search failed: {e}")),
    }
}

inventory::submit! {
    McpExtensionTool {
        name: "helios_graphrag_search",
        description: "Cross-modal seed → BFS expand → return query over _hdb_graph_*. \
                      Substring-matches `seed_text` against node titles/text, then \
                      walks the graph up to `hops` deep through optional `edge_kinds`.",
        schema: graphrag_search_schema,
        handler: graphrag_search_handler,
    }
}

fn graphrag_search_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "seed_text":  { "type": "string", "description": "Substring to match (case-insensitive) against title/text" },
            "seed_kinds": { "type": "array",  "items": { "type": "string" }, "description": "Optional: restrict seeds to these node_kind values" },
            "hops":       { "type": "integer", "default": 2 },
            "edge_kinds": { "type": "array",  "items": { "type": "string" }, "description": "Optional: only traverse these edge_kind values" },
            "direction":  { "type": "string", "enum": ["out", "in", "both"], "default": "both" },
            "limit":      { "type": "integer", "default": 50 }
        },
        "required": ["seed_text"]
    })
}

#[cfg(test)]
mod tests {
    use super::super::auto_register::{registered, try_call};
    use serde_json::json;

    #[test]
    fn graphrag_search_is_registered() {
        let names: Vec<_> = registered().map(|t| t.name).collect();
        assert!(names.contains(&"helios_graphrag_search"), "have: {names:?}");
    }

    #[test]
    fn missing_db_errors() {
        let r = try_call(None, "helios_graphrag_search", json!({ "seed_text": "x" }))
            .expect("matched");
        assert!(r.is_error);
        assert!(r.payload["error"].as_str().unwrap().contains("requires a database"));
    }
}
