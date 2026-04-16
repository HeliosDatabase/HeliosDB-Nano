//! Tool handler implementations for the HelixDB-inspired MCP extensions.

use std::sync::Arc;

use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::graph::{
    sql as graph_sql,
    storage::{Edge, GraphStore},
};
use crate::search::{hybrid::bm25_hits, hybrid_search, Bm25Index, FusionMethod, ScoredHit};

/// Process-wide BM25 indexes keyed by user-supplied name.
pub static BM25_INDEXES: Lazy<DashMap<String, Arc<Bm25Index>>> = Lazy::new(DashMap::new);

/// Process-wide graph store -- shared by graph_traverse / graph_path.
pub static GRAPH_STORE: Lazy<Arc<GraphStore>> = Lazy::new(|| Arc::new(GraphStore::new()));

/// MCP-style tool descriptor: name + JSON-schema input shape.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// Result of a tool invocation -- mirrors MCP's `ToolResult` shape but
/// trimmed to what the standalone module needs.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub is_error: bool,
    pub payload: Value,
}

impl ToolOutcome {
    pub fn ok(v: Value) -> Self {
        Self {
            is_error: false,
            payload: v,
        }
    }

    pub fn err<E: ToString>(e: E) -> Self {
        Self {
            is_error: true,
            payload: json!({ "error": e.to_string() }),
        }
    }
}

/// Catalog of every tool exposed by this extension module.
#[must_use]
pub fn list_tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "heliosdb_bm25_index",
            description: "Create or replace an in-memory BM25 index from a list of (doc_id, text) documents.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "documents": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "doc_id": { "type": "integer" },
                                "text":   { "type": "string"  }
                            },
                            "required": ["doc_id", "text"]
                        }
                    }
                },
                "required": ["name", "documents"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_hybrid_search",
            description: "Hybrid BM25 + vector search with RRF / MMR / weighted-linear fusion.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "index_name": { "type": "string" },
                    "query_text": { "type": "string" },
                    "vector_hits": { "type": "array", "default": [] },
                    "fusion": { "type": "string", "enum": ["rrf", "mmr", "linear"], "default": "rrf" },
                    "lambda": { "type": "number", "default": 0.5 },
                    "limit":  { "type": "integer", "default": 10 }
                },
                "required": ["index_name", "query_text"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_graph_add_edge",
            description: "Add a directed edge to the in-process graph store.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from":   { "type": "string" },
                    "to":     { "type": "string" },
                    "label":  { "type": "string", "default": "edge" },
                    "weight": { "type": "number", "default": 1.0 }
                },
                "required": ["from", "to"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_graph_traverse",
            description: "BFS traversal from a starting node, with optional label filter and depth bound.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "start":      { "type": "string" },
                    "edge_label": { "type": "string" },
                    "direction":  { "type": "string", "default": "out" },
                    "depth":      { "type": "integer", "default": 3 }
                },
                "required": ["start"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_graph_path",
            description: "Shortest-path query using BFS, Dijkstra, or bidirectional BFS.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from":       { "type": "string" },
                    "to":         { "type": "string" },
                    "algorithm":  { "type": "string", "default": "bfs" },
                    "direction":  { "type": "string", "default": "out" },
                    "edge_label": { "type": "string" }
                },
                "required": ["from", "to"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_embed_and_store",
            description: "Stash a (doc_id, text) tuple into a BM25 index (auto-created on first call).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "index_name": { "type": "string" },
                    "doc_id":     { "type": "integer" },
                    "text":       { "type": "string" }
                },
                "required": ["index_name", "doc_id", "text"]
            }),
        },
    ]
}

/// Dispatch a tool call by name.
pub fn call_tool(name: &str, args: Value) -> ToolOutcome {
    match name {
        "heliosdb_bm25_index" => bm25_index(args),
        "heliosdb_hybrid_search" => do_hybrid_search(args),
        "heliosdb_graph_add_edge" => graph_add_edge(args),
        "heliosdb_graph_traverse" => graph_traverse(args),
        "heliosdb_graph_path" => graph_path(args),
        "heliosdb_embed_and_store" => embed_and_store(args),
        other => ToolOutcome::err(format!("unknown tool '{other}'")),
    }
}

// ---- Tool input structs ------------------------------------------------

#[derive(Debug, Deserialize)]
struct Bm25IndexInput {
    name: String,
    documents: Vec<Bm25Doc>,
}
#[derive(Debug, Deserialize)]
struct Bm25Doc {
    doc_id: u64,
    text: String,
}

#[derive(Debug, Deserialize)]
struct HybridInput {
    index_name: String,
    query_text: String,
    #[serde(default)]
    vector_hits: Vec<HybridVecHit>,
    #[serde(default = "default_fusion")]
    fusion: String,
    #[serde(default = "default_lambda")]
    lambda: f64,
    #[serde(default = "default_limit")]
    limit: usize,
}
#[derive(Debug, Deserialize)]
struct HybridVecHit {
    doc_id: u64,
    score: f64,
    #[serde(default)]
    vector: Option<Vec<f32>>,
}

#[derive(Debug, Deserialize)]
struct GraphAddEdgeInput {
    from: String,
    to: String,
    #[serde(default = "default_label")]
    label: String,
    #[serde(default = "default_weight")]
    weight: f64,
}

#[derive(Debug, Deserialize)]
struct GraphTraverseInput {
    start: String,
    #[serde(default)]
    edge_label: Option<String>,
    #[serde(default = "default_direction")]
    direction: String,
    #[serde(default = "default_depth")]
    depth: usize,
}

#[derive(Debug, Deserialize)]
struct GraphPathInput {
    from: String,
    to: String,
    #[serde(default = "default_algorithm")]
    algorithm: String,
    #[serde(default = "default_direction")]
    direction: String,
    #[serde(default)]
    edge_label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EmbedAndStoreInput {
    index_name: String,
    doc_id: u64,
    text: String,
}

fn default_fusion() -> String {
    "rrf".to_string()
}
fn default_lambda() -> f64 {
    0.5
}
fn default_limit() -> usize {
    10
}
fn default_label() -> String {
    "edge".to_string()
}
fn default_weight() -> f64 {
    1.0
}
fn default_direction() -> String {
    "out".to_string()
}
fn default_depth() -> usize {
    3
}
fn default_algorithm() -> String {
    "bfs".to_string()
}

// ---- Handlers ----------------------------------------------------------

fn bm25_index(args: Value) -> ToolOutcome {
    let input: Bm25IndexInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let idx = Arc::new(Bm25Index::new());
    for d in &input.documents {
        idx.add_document(d.doc_id, &d.text);
    }
    let count = input.documents.len();
    BM25_INDEXES.insert(input.name.clone(), idx);
    ToolOutcome::ok(json!({
        "index": input.name,
        "indexed_documents": count,
    }))
}

fn do_hybrid_search(args: Value) -> ToolOutcome {
    let input: HybridInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let Some(idx) = BM25_INDEXES.get(&input.index_name) else {
        return ToolOutcome::err(format!(
            "BM25 index '{}' not found -- create one via heliosdb_bm25_index first",
            input.index_name
        ));
    };
    let bm25 = bm25_hits(idx.value(), &input.query_text, Some(input.limit * 4));
    let vec_hits: Vec<ScoredHit> = input
        .vector_hits
        .into_iter()
        .map(|h| ScoredHit {
            doc_id: h.doc_id,
            score: h.score,
            vector: h.vector,
        })
        .collect();
    let fusion = match input.fusion.to_ascii_lowercase().as_str() {
        "rrf" => FusionMethod::Rrf,
        "mmr" => FusionMethod::Mmr,
        "linear" => FusionMethod::Linear,
        other => return ToolOutcome::err(format!("unknown fusion method '{other}'")),
    };
    let res = hybrid_search(&bm25, &vec_hits, fusion, input.lambda, input.limit);
    ToolOutcome::ok(json!({
        "index": input.index_name,
        "fusion": input.fusion,
        "results": res
            .iter()
            .map(|h| json!({"doc_id": h.doc_id, "score": h.score}))
            .collect::<Vec<_>>(),
    }))
}

fn graph_add_edge(args: Value) -> ToolOutcome {
    let input: GraphAddEdgeInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let from = match uuid::Uuid::parse_str(&input.from) {
        Ok(u) => u,
        Err(e) => return ToolOutcome::err(format!("invalid 'from' uuid: {e}")),
    };
    let to = match uuid::Uuid::parse_str(&input.to) {
        Ok(u) => u,
        Err(e) => return ToolOutcome::err(format!("invalid 'to' uuid: {e}")),
    };
    let id = GRAPH_STORE.add_edge(Edge::new(from, to, input.label).with_weight(input.weight));
    ToolOutcome::ok(json!({
        "edge_id": id.to_string(),
        "from": from.to_string(),
        "to": to.to_string(),
        "edge_count": GRAPH_STORE.edge_count(),
    }))
}

fn graph_traverse(args: Value) -> ToolOutcome {
    let input: GraphTraverseInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let start = match uuid::Uuid::parse_str(&input.start) {
        Ok(u) => u,
        Err(e) => return ToolOutcome::err(format!("invalid 'start' uuid: {e}")),
    };
    let direction = match graph_sql::parse_direction(&input.direction) {
        Ok(d) => d,
        Err(e) => return ToolOutcome::err(e.to_string()),
    };
    let rows = graph_sql::graph_traverse(&GRAPH_STORE, start, input.edge_label.as_deref(), direction, input.depth);
    ToolOutcome::ok(json!({
        "start": start.to_string(),
        "direction": format!("{:?}", direction),
        "rows": rows
            .iter()
            .map(|r| json!({"node": r.node.to_string(), "depth": r.depth}))
            .collect::<Vec<_>>(),
    }))
}

fn graph_path(args: Value) -> ToolOutcome {
    let input: GraphPathInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let from = match uuid::Uuid::parse_str(&input.from) {
        Ok(u) => u,
        Err(e) => return ToolOutcome::err(format!("invalid 'from' uuid: {e}")),
    };
    let to = match uuid::Uuid::parse_str(&input.to) {
        Ok(u) => u,
        Err(e) => return ToolOutcome::err(format!("invalid 'to' uuid: {e}")),
    };
    let direction = match graph_sql::parse_direction(&input.direction) {
        Ok(d) => d,
        Err(e) => return ToolOutcome::err(e.to_string()),
    };
    let algorithm = match graph_sql::parse_algorithm(&input.algorithm) {
        Ok(a) => a,
        Err(e) => return ToolOutcome::err(e.to_string()),
    };
    let path = graph_sql::graph_shortest_path(
        &GRAPH_STORE,
        from,
        to,
        algorithm,
        direction,
        input.edge_label.as_deref(),
    );
    match path {
        Some(p) => ToolOutcome::ok(json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "algorithm": input.algorithm,
            "hops": p.hops(),
            "total_weight": p.total_weight,
            "nodes": p.nodes.iter().map(uuid::Uuid::to_string).collect::<Vec<_>>(),
            "edges": p.edges.iter().map(uuid::Uuid::to_string).collect::<Vec<_>>(),
        })),
        None => ToolOutcome::ok(json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "path_found": false,
        })),
    }
}

fn embed_and_store(args: Value) -> ToolOutcome {
    let input: EmbedAndStoreInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let idx = BM25_INDEXES
        .entry(input.index_name.clone())
        .or_insert_with(|| Arc::new(Bm25Index::new()))
        .value()
        .clone();
    idx.add_document(input.doc_id, &input.text);
    ToolOutcome::ok(json!({
        "index": input.index_name,
        "doc_id": input.doc_id,
        "bm25": "indexed",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_index_name(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn list_tools_includes_all_six() {
        let names: Vec<_> = list_tools().into_iter().map(|t| t.name).collect();
        for n in [
            "heliosdb_bm25_index",
            "heliosdb_hybrid_search",
            "heliosdb_graph_add_edge",
            "heliosdb_graph_traverse",
            "heliosdb_graph_path",
            "heliosdb_embed_and_store",
        ] {
            assert!(names.contains(&n), "missing {n} in {names:?}");
        }
    }

    #[test]
    fn bm25_index_then_search() {
        let name = unique_index_name("unit-bm25");
        let r = call_tool(
            "heliosdb_bm25_index",
            json!({
                "name": name,
                "documents": [
                    {"doc_id": 1, "text": "alpha beta"},
                    {"doc_id": 2, "text": "gamma delta"},
                ]
            }),
        );
        assert!(!r.is_error);
        assert_eq!(r.payload["indexed_documents"].as_u64(), Some(2));

        let r2 = call_tool(
            "heliosdb_hybrid_search",
            json!({
                "index_name": name,
                "query_text": "alpha",
                "fusion": "rrf",
                "limit": 5
            }),
        );
        assert!(!r2.is_error);
        let arr = r2.payload["results"].as_array().expect("results");
        assert_eq!(arr.first().and_then(|x| x["doc_id"].as_u64()), Some(1));
    }

    #[test]
    fn hybrid_search_unknown_index_errors() {
        let r = call_tool(
            "heliosdb_hybrid_search",
            json!({"index_name": "definitely-missing", "query_text": "x"}),
        );
        assert!(r.is_error);
    }

    #[test]
    fn unknown_tool_errors() {
        let r = call_tool("not_a_tool", json!({}));
        assert!(r.is_error);
    }

    #[test]
    fn graph_invalid_uuid_errors() {
        let r = call_tool(
            "heliosdb_graph_add_edge",
            json!({"from": "nope", "to": uuid::Uuid::new_v4().to_string()}),
        );
        assert!(r.is_error);
    }

    #[test]
    fn embed_and_store_indexes_then_search_finds_it() {
        let name = unique_index_name("unit-embed");
        let r = call_tool(
            "heliosdb_embed_and_store",
            json!({"index_name": name, "doc_id": 99, "text": "needle in haystack"}),
        );
        assert!(!r.is_error);

        let r2 = call_tool(
            "heliosdb_hybrid_search",
            json!({"index_name": name, "query_text": "needle", "fusion": "rrf", "limit": 5}),
        );
        let arr = r2.payload["results"].as_array().expect("results");
        assert!(arr.iter().any(|h| h["doc_id"].as_u64() == Some(99)));
    }
}
