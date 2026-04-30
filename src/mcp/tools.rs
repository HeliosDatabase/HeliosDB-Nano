//! Unified MCP tool catalogue for HeliosDB-Nano.
//!
//! Combines two families of tools under a single dispatcher:
//!
//! * **DB-backed** (`heliosdb_query`, `heliosdb_schema`, `heliosdb_list_tables`,
//!   `heliosdb_create_table`, `heliosdb_insert`, `heliosdb_branch_create`,
//!   `heliosdb_branch_list`, `heliosdb_branch_merge`, `heliosdb_search`,
//!   `heliosdb_time_travel`) — require an `EmbeddedDatabase` reference.
//! * **In-process** (`heliosdb_bm25_index`, `heliosdb_hybrid_search`,
//!   `heliosdb_graph_add_edge`, `heliosdb_graph_traverse`, `heliosdb_graph_path`,
//!   `heliosdb_embed_and_store`) — use process-static BM25 / graph state
//!   so they work from transports that don't thread a DB through.
//!
//! The dispatcher takes `Option<&EmbeddedDatabase>` so the same entry
//! point is reusable from the stdio server (has DB) and from HTTP
//! handlers that may choose to expose only the in-process subset.

use std::sync::Arc;

use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::graph::{
    sql as graph_sql,
    storage::{Edge, GraphStore},
};
use crate::search::{hybrid::bm25_hits, hybrid_search, Bm25Index, FusionMethod, ScoredHit};
use crate::{EmbeddedDatabase, Tuple, Value};

use super::protocol::Tool;

/// Process-wide BM25 indexes keyed by user-supplied name.
pub static BM25_INDEXES: Lazy<DashMap<String, Arc<Bm25Index>>> = Lazy::new(DashMap::new);

/// Process-wide graph store used by `graph_traverse` / `graph_path`.
pub static GRAPH_STORE: Lazy<Arc<GraphStore>> = Lazy::new(|| Arc::new(GraphStore::new()));

/// MCP-style tool descriptor: name, human description, JSON-schema input.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: JsonValue,
}

impl ToolDescriptor {
    pub fn to_tool(&self) -> Tool {
        Tool {
            name: self.name.to_string(),
            description: self.description.to_string(),
            input_schema: self.input_schema.clone(),
        }
    }
}

/// Result of a tool invocation.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub is_error: bool,
    pub payload: JsonValue,
}

impl ToolOutcome {
    pub fn ok(v: JsonValue) -> Self {
        Self { is_error: false, payload: v }
    }
    pub fn err<E: ToString>(e: E) -> Self {
        Self { is_error: true, payload: json!({ "error": e.to_string() }) }
    }
}

/// Full catalogue of tools — DB-backed and in-process alike. The stdio
/// server advertises all of them; an HTTP handler without a DB can still
/// list them but will error out on DB-backed `tools/call` requests.
///
/// Inventory-registered tools (declared via `mcp_tool!` from any module
/// gated on `mcp-endpoint`) are merged in here, so adding a new tool
/// from outside `tools.rs` doesn't require editing this file.
#[must_use]
pub fn list_tools() -> Vec<ToolDescriptor> {
    let mut out = db_tools();
    out.extend(in_process_tools());
    for entry in super::auto_register::registered() {
        out.push(ToolDescriptor {
            name: entry.name,
            description: entry.description,
            input_schema: (entry.schema)(),
        });
    }
    out
}

fn db_tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "heliosdb_query",
            description: "Execute a SQL query against the database. Returns rows as JSON arrays.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string" },
                    "params": { "type": "array", "items": {}, "default": [] },
                    "branch": { "type": "string", "default": "main" }
                },
                "required": ["sql"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_schema",
            description: "Return the column list of a given table from the catalog.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "table":  { "type": "string" },
                    "branch": { "type": "string", "default": "main" }
                },
                "required": ["table"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_list_tables",
            description: "List all user tables (filters out helios_* / mv_* internals).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "branch": { "type": "string", "default": "main" }
                }
            }),
        },
        ToolDescriptor {
            name: "heliosdb_create_table",
            description: "Create a new table with the given columns.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "columns": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name":        { "type": "string" },
                                "type":        { "type": "string" },
                                "nullable":    { "type": "boolean", "default": true },
                                "primary_key": { "type": "boolean", "default": false }
                            },
                            "required": ["name", "type"]
                        }
                    },
                    "branch": { "type": "string", "default": "main" }
                },
                "required": ["name", "columns"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_insert",
            description: "Insert one or more rows into a table.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "table":  { "type": "string" },
                    "rows":   { "type": "array", "items": { "type": "object" } },
                    "branch": { "type": "string", "default": "main" }
                },
                "required": ["table", "rows"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_branch_create",
            description: "Create a new branch (copy-on-write) starting from an existing one.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name":        { "type": "string" },
                    "from_branch": { "type": "string", "default": "main" }
                },
                "required": ["name"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_branch_list",
            description: "List every branch known to the database.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDescriptor {
            name: "heliosdb_branch_merge",
            description: "Merge a source branch into a target branch.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string" },
                    "target": { "type": "string", "default": "main" }
                },
                "required": ["source"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_search",
            description: "Vector-similarity search over a table with an embedding column.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "table":         { "type": "string" },
                    "vector":        { "type": "array", "items": { "type": "number" } },
                    "vector_column": { "type": "string", "default": "embedding" },
                    "top_k":         { "type": "integer", "default": 10 },
                    "branch":        { "type": "string", "default": "main" }
                },
                "required": ["table", "vector"]
            }),
        },
        ToolDescriptor {
            name: "heliosdb_time_travel",
            description: "Run a read-only query against a historical snapshot via AS OF TIMESTAMP.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql":       { "type": "string" },
                    "timestamp": { "type": "string", "description": "ISO-8601 timestamp" },
                    "branch":    { "type": "string", "default": "main" }
                },
                "required": ["sql", "timestamp"]
            }),
        },
    ]
}

fn in_process_tools() -> Vec<ToolDescriptor> {
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
                    "index_name":  { "type": "string" },
                    "query_text":  { "type": "string" },
                    "vector_hits": { "type": "array", "default": [] },
                    "fusion":      { "type": "string", "enum": ["rrf", "mmr", "linear"], "default": "rrf" },
                    "lambda":      { "type": "number",  "default": 0.5 },
                    "limit":       { "type": "integer", "default": 10 }
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

/// Unified dispatch. In-process tools ignore `db`; DB-backed tools error
/// with a clear message when `db` is `None`.
///
/// Wrapped in a server-side LRU cache for read-only tools (see
/// `super::result_cache`) — repeated identical calls inside an
/// agentic-coding session return the cached `ToolOutcome` without
/// re-running the handler. Cache invalidates on any mutating tool call
/// (`super::result_cache::writes`) and on TTL expiry.
pub fn call_tool(db: Option<&EmbeddedDatabase>, name: &str, args: JsonValue) -> ToolOutcome {
    use super::result_cache;

    // Cache-hit short-circuit. Read-only tools only.
    if let Some(cached) = result_cache::try_get(name, &args) {
        return cached;
    }

    let outcome = call_tool_inner(db, name, args.clone());

    // Populate cache (no-op for non-read-only tools or error outcomes).
    result_cache::insert(name, &args, &outcome);

    // Bump generation on writes — invalidates everything cached so far.
    if !outcome.is_error && result_cache::writes(name) {
        result_cache::invalidate_for_writes();
    }

    outcome
}

fn call_tool_inner(db: Option<&EmbeddedDatabase>, name: &str, args: JsonValue) -> ToolOutcome {
    match name {
        "heliosdb_query" => require_db(db, "heliosdb_query", |d| do_query(d, args)),
        "heliosdb_schema" => require_db(db, "heliosdb_schema", |d| do_schema(d, args)),
        "heliosdb_list_tables" => require_db(db, "heliosdb_list_tables", |d| do_list_tables(d, args)),
        "heliosdb_create_table" => require_db(db, "heliosdb_create_table", |d| do_create_table(d, args)),
        "heliosdb_insert" => require_db(db, "heliosdb_insert", |d| do_insert(d, args)),
        "heliosdb_branch_create" => require_db(db, "heliosdb_branch_create", |d| do_branch_create(d, args)),
        "heliosdb_branch_list" => require_db(db, "heliosdb_branch_list", |d| do_branch_list(d)),
        "heliosdb_branch_merge" => require_db(db, "heliosdb_branch_merge", |d| do_branch_merge(d, args)),
        "heliosdb_search" => require_db(db, "heliosdb_search", |d| do_search(d, args)),
        "heliosdb_time_travel" => require_db(db, "heliosdb_time_travel", |d| do_time_travel(d, args)),

        "heliosdb_bm25_index" => do_bm25_index(args),
        "heliosdb_hybrid_search" => do_hybrid_search(args),
        "heliosdb_graph_add_edge" => do_graph_add_edge(args),
        "heliosdb_graph_traverse" => do_graph_traverse(args),
        "heliosdb_graph_path" => do_graph_path(args),
        "heliosdb_embed_and_store" => do_embed_and_store(args),

        other => super::auto_register::try_call(db, other, args)
            .unwrap_or_else(|| ToolOutcome::err(format!("unknown tool '{other}'"))),
    }
}

fn require_db<F>(db: Option<&EmbeddedDatabase>, tool: &str, f: F) -> ToolOutcome
where
    F: FnOnce(&EmbeddedDatabase) -> ToolOutcome,
{
    match db {
        Some(d) => f(d),
        None => ToolOutcome::err(format!(
            "tool '{tool}' requires a database connection; this transport is in-process-only"
        )),
    }
}

// ---- Branch scoping helper --------------------------------------------

/// Run `f` with the active branch set to `branch`, restoring the previous
/// branch afterwards. `branch == "main"` and `branch == current` both
/// skip the switch.
fn with_branch<F, R>(db: &EmbeddedDatabase, branch: &str, f: F) -> crate::Result<R>
where
    F: FnOnce(&EmbeddedDatabase) -> crate::Result<R>,
{
    let previous = db.storage.get_current_branch();
    let current = previous.as_deref().unwrap_or("main");
    if branch == current {
        return f(db);
    }
    if branch != "main" {
        db.switch_branch(branch)?;
    } else {
        // "main" requested while on a non-main branch: switch off.
        db.switch_branch("main")?;
    }
    let result = f(db);
    // Restore — best-effort.
    if let Some(prev) = previous {
        if prev != branch {
            let _ = db.switch_branch(&prev);
        }
    } else if branch != "main" {
        let _ = db.switch_branch("main");
    }
    result
}

// ---- DB tool input structs --------------------------------------------

#[derive(Debug, Deserialize)]
struct QueryInput {
    sql: String,
    #[serde(default)]
    params: Vec<JsonValue>,
    #[serde(default = "default_branch")]
    branch: String,
}

#[derive(Debug, Deserialize)]
struct SchemaInput {
    table: String,
    #[serde(default = "default_branch")]
    branch: String,
}

#[derive(Debug, Deserialize)]
struct ListTablesInput {
    #[serde(default = "default_branch")]
    branch: String,
}

#[derive(Debug, Deserialize)]
struct CreateTableInput {
    name: String,
    columns: Vec<ColumnDef>,
    #[serde(default = "default_branch")]
    branch: String,
}

#[derive(Debug, Deserialize)]
struct ColumnDef {
    name: String,
    #[serde(rename = "type")]
    col_type: String,
    #[serde(default = "default_true")]
    nullable: bool,
    #[serde(default)]
    primary_key: bool,
}

#[derive(Debug, Deserialize)]
struct InsertInput {
    table: String,
    rows: Vec<serde_json::Map<String, JsonValue>>,
    #[serde(default = "default_branch")]
    branch: String,
}

#[derive(Debug, Deserialize)]
struct BranchCreateInput {
    name: String,
    #[serde(default = "default_branch")]
    from_branch: String,
}

#[derive(Debug, Deserialize)]
struct BranchMergeInput {
    source: String,
    #[serde(default = "default_branch")]
    target: String,
}

#[derive(Debug, Deserialize)]
struct SearchInput {
    table: String,
    vector: Vec<f32>,
    #[serde(default = "default_vector_column")]
    vector_column: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
    #[serde(default = "default_branch")]
    branch: String,
}

#[derive(Debug, Deserialize)]
struct TimeTravelInput {
    sql: String,
    timestamp: String,
    #[serde(default = "default_branch")]
    branch: String,
}

fn default_branch() -> String { "main".to_string() }
fn default_vector_column() -> String { "embedding".to_string() }
fn default_top_k() -> usize { 10 }
fn default_true() -> bool { true }

// ---- DB-backed handlers ------------------------------------------------

fn do_query(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: QueryInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let params: Vec<Value> = input.params.iter().map(json_to_value).collect();
    let run = |d: &EmbeddedDatabase| d.query_params(&input.sql, &params);
    let rows = match with_branch(db, &input.branch, run) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::err(format!("query failed: {e}")),
    };
    ToolOutcome::ok(json!({
        "branch": input.branch,
        "row_count": rows.len(),
        "rows": rows.iter().map(tuple_to_json).collect::<Vec<_>>(),
    }))
}

fn do_schema(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: SchemaInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let lookup = |d: &EmbeddedDatabase| d.storage.catalog().get_table_schema(&input.table);
    let schema = match with_branch(db, &input.branch, lookup) {
        Ok(s) => s,
        Err(e) => return ToolOutcome::err(format!("schema lookup failed: {e}")),
    };
    let cols: Vec<JsonValue> = schema
        .columns
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "data_type": format!("{:?}", c.data_type),
                "nullable": c.nullable,
                "primary_key": c.primary_key,
            })
        })
        .collect();
    ToolOutcome::ok(json!({
        "table": input.table,
        "branch": input.branch,
        "columns": cols,
    }))
}

fn do_list_tables(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: ListTablesInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let run = |d: &EmbeddedDatabase| d.storage.catalog().list_tables();
    let names = match with_branch(db, &input.branch, run) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::err(format!("list tables failed: {e}")),
    };
    let user_tables: Vec<String> = names
        .into_iter()
        .filter(|n| !n.starts_with("helios_") && !n.starts_with("mv_"))
        .collect();
    ToolOutcome::ok(json!({ "branch": input.branch, "tables": user_tables }))
}

fn do_create_table(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: CreateTableInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let columns: Vec<String> = input
        .columns
        .iter()
        .map(|c| {
            let mut s = format!("{} {}", c.name, c.col_type);
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
            if c.primary_key {
                s.push_str(" PRIMARY KEY");
            }
            s
        })
        .collect();
    let sql = format!("CREATE TABLE {} ({})", input.name, columns.join(", "));
    let run = |d: &EmbeddedDatabase| d.execute(&sql);
    match with_branch(db, &input.branch, run) {
        Ok(_) => ToolOutcome::ok(json!({
            "table": input.name,
            "branch": input.branch,
            "created": true,
        })),
        Err(e) => ToolOutcome::err(format!("create table failed: {e}")),
    }
}

fn do_insert(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: InsertInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    if input.rows.is_empty() {
        return ToolOutcome::err("no rows to insert");
    }
    let run = |d: &EmbeddedDatabase| -> crate::Result<usize> {
        let mut inserted = 0usize;
        for row in &input.rows {
            let columns: Vec<&str> = row.keys().map(String::as_str).collect();
            let placeholders: Vec<String> =
                (1..=columns.len()).map(|i| format!("${i}")).collect();
            let values: Vec<Value> = row.values().map(json_to_value).collect();
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                input.table,
                columns.join(", "),
                placeholders.join(", ")
            );
            d.execute_params(&sql, &values)?;
            inserted += 1;
        }
        Ok(inserted)
    };
    match with_branch(db, &input.branch, run) {
        Ok(n) => ToolOutcome::ok(json!({
            "table": input.table,
            "branch": input.branch,
            "inserted": n,
        })),
        Err(e) => ToolOutcome::err(format!("insert failed: {e}")),
    }
}

fn do_branch_create(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: BranchCreateInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    // Create from the requested parent branch: switch to it first, then
    // create. Restore afterwards.
    let parent_scope = |d: &EmbeddedDatabase| d.create_branch(&input.name);
    match with_branch(db, &input.from_branch, parent_scope) {
        Ok(_) => ToolOutcome::ok(json!({
            "branch": input.name,
            "from": input.from_branch,
            "created": true,
        })),
        Err(e) => ToolOutcome::err(format!("branch create failed: {e}")),
    }
}

fn do_branch_list(db: &EmbeddedDatabase) -> ToolOutcome {
    match db.storage.list_branches() {
        Ok(rows) => {
            let names: Vec<String> = rows.into_iter().map(|b| b.name).collect();
            ToolOutcome::ok(json!({ "branches": names }))
        }
        Err(e) => ToolOutcome::err(format!("list branches failed: {e}")),
    }
}

fn do_branch_merge(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: BranchMergeInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    // merge_branch merges `source` into the *current* branch; switch to
    // `target` first, then merge.
    let scope = |d: &EmbeddedDatabase| d.merge_branch(&input.source);
    match with_branch(db, &input.target, scope) {
        Ok(_) => ToolOutcome::ok(json!({
            "source": input.source,
            "target": input.target,
            "merged": true,
        })),
        Err(e) => ToolOutcome::err(format!("merge failed: {e}")),
    }
}

fn do_search(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: SearchInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    let sql = format!(
        "SELECT *, vector_distance({col}, $1) AS distance \
         FROM {tbl} ORDER BY distance LIMIT {k}",
        col = input.vector_column,
        tbl = input.table,
        k = input.top_k,
    );
    let params = vec![Value::Vector(input.vector)];
    let run = |d: &EmbeddedDatabase| d.query_params(&sql, &params);
    match with_branch(db, &input.branch, run) {
        Ok(rows) => ToolOutcome::ok(json!({
            "table": input.table,
            "branch": input.branch,
            "count": rows.len(),
            "results": rows.iter().map(tuple_to_json).collect::<Vec<_>>(),
        })),
        Err(e) => ToolOutcome::err(format!("search failed: {e}")),
    }
}

fn do_time_travel(db: &EmbeddedDatabase, args: JsonValue) -> ToolOutcome {
    let input: TimeTravelInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolOutcome::err(format!("invalid arguments: {e}")),
    };
    // Nano rewrites `AS OF TIMESTAMP 'iso'` natively; splice it onto the
    // user's SQL so the query runs against the historical snapshot.
    let escaped = input.timestamp.replace('\'', "''");
    let rewritten = format!("{} AS OF TIMESTAMP '{}'", input.sql.trim_end_matches(';'), escaped);
    let run = |d: &EmbeddedDatabase| d.query(&rewritten, &[]);
    match with_branch(db, &input.branch, run) {
        Ok(rows) => ToolOutcome::ok(json!({
            "branch": input.branch,
            "timestamp": input.timestamp,
            "count": rows.len(),
            "rows": rows.iter().map(tuple_to_json).collect::<Vec<_>>(),
        })),
        Err(e) => ToolOutcome::err(format!("time-travel query failed: {e}")),
    }
}

// ---- In-process tool input structs ------------------------------------

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
    #[serde(default = "default_edge_label")]
    label: String,
    #[serde(default = "default_edge_weight")]
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

fn default_fusion() -> String { "rrf".to_string() }
fn default_lambda() -> f64 { 0.5 }
fn default_limit() -> usize { 10 }
fn default_edge_label() -> String { "edge".to_string() }
fn default_edge_weight() -> f64 { 1.0 }
fn default_direction() -> String { "out".to_string() }
fn default_depth() -> usize { 3 }
fn default_algorithm() -> String { "bfs".to_string() }

// ---- In-process handlers ----------------------------------------------

fn do_bm25_index(args: JsonValue) -> ToolOutcome {
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

fn do_hybrid_search(args: JsonValue) -> ToolOutcome {
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
        .map(|h| ScoredHit { doc_id: h.doc_id, score: h.score, vector: h.vector })
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
            .map(|h| json!({ "doc_id": h.doc_id, "score": h.score }))
            .collect::<Vec<_>>(),
    }))
}

fn do_graph_add_edge(args: JsonValue) -> ToolOutcome {
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

fn do_graph_traverse(args: JsonValue) -> ToolOutcome {
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
    let rows = graph_sql::graph_traverse(
        &GRAPH_STORE,
        start,
        input.edge_label.as_deref(),
        direction,
        input.depth,
    );
    ToolOutcome::ok(json!({
        "start": start.to_string(),
        "direction": format!("{direction:?}"),
        "rows": rows
            .iter()
            .map(|r| json!({ "node": r.node.to_string(), "depth": r.depth }))
            .collect::<Vec<_>>(),
    }))
}

fn do_graph_path(args: JsonValue) -> ToolOutcome {
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

fn do_embed_and_store(args: JsonValue) -> ToolOutcome {
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

// ---- JSON <-> Value conversions ---------------------------------------

pub(crate) fn tuple_to_json(t: &Tuple) -> JsonValue {
    JsonValue::Array(t.values.iter().map(value_to_json).collect())
}

pub(crate) fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Null => JsonValue::Null,
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::Int2(n) => JsonValue::from(*n),
        Value::Int4(n) => JsonValue::from(*n),
        Value::Int8(n) => JsonValue::from(*n),
        Value::Float4(n) => {
            serde_json::Number::from_f64(f64::from(*n))
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null)
        }
        Value::Float8(n) => {
            serde_json::Number::from_f64(*n)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null)
        }
        Value::Numeric(s) | Value::String(s) => JsonValue::String(s.clone()),
        Value::Bytes(b) => {
            use base64::{engine::general_purpose::STANDARD as B64, Engine};
            JsonValue::String(B64.encode(b))
        }
        Value::Uuid(u) => JsonValue::String(u.to_string()),
        Value::Timestamp(t) => JsonValue::String(t.to_rfc3339()),
        Value::Date(d) => JsonValue::String(d.to_string()),
        Value::Time(t) => JsonValue::String(t.to_string()),
        Value::Interval(i) => JsonValue::from(*i),
        Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| JsonValue::String(s.clone())),
        Value::Array(a) => JsonValue::Array(a.iter().map(value_to_json).collect()),
        Value::Vector(v) => JsonValue::Array(
            v.iter()
                .map(|f| {
                    serde_json::Number::from_f64(f64::from(*f))
                        .map(JsonValue::Number)
                        .unwrap_or(JsonValue::Null)
                })
                .collect(),
        ),
        Value::DictRef { dict_id } => json!({ "dict_id": dict_id }),
        Value::CasRef { hash } => json!({ "cas_ref": hex::encode(hash) }),
        Value::ColumnarRef => JsonValue::String("<columnar_ref>".to_string()),
    }
}

pub(crate) fn json_to_value(v: &JsonValue) -> Value {
    match v {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(b) => Value::Boolean(*b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int8(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float8(f)
            } else {
                Value::Null
            }
        }
        JsonValue::String(s) => Value::String(s.clone()),
        JsonValue::Array(arr) => {
            let floats: Result<Vec<f32>, ()> = arr
                .iter()
                .map(|v| v.as_f64().map(|f| f as f32).ok_or(()))
                .collect();
            if let Ok(vec) = floats {
                Value::Vector(vec)
            } else {
                Value::String(serde_json::to_string(arr).unwrap_or_default())
            }
        }
        JsonValue::Object(_) => Value::String(serde_json::to_string(v).unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn list_tools_covers_db_and_in_process() {
        let names: Vec<_> = list_tools().into_iter().map(|t| t.name).collect();
        for n in [
            "heliosdb_query",
            "heliosdb_schema",
            "heliosdb_list_tables",
            "heliosdb_create_table",
            "heliosdb_insert",
            "heliosdb_branch_create",
            "heliosdb_branch_list",
            "heliosdb_branch_merge",
            "heliosdb_search",
            "heliosdb_time_travel",
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
    fn db_tool_without_db_errors_cleanly() {
        let r = call_tool(None, "heliosdb_query", json!({ "sql": "SELECT 1" }));
        assert!(r.is_error);
        assert!(r.payload["error"].as_str().unwrap().contains("requires a database"));
    }

    #[test]
    fn in_process_tool_works_without_db() {
        let name = unique("tools-unit");
        let r = call_tool(
            None,
            "heliosdb_bm25_index",
            json!({
                "name": name,
                "documents": [
                    { "doc_id": 1, "text": "alpha beta" },
                    { "doc_id": 2, "text": "gamma delta" }
                ]
            }),
        );
        assert!(!r.is_error);
        assert_eq!(r.payload["indexed_documents"].as_u64(), Some(2));
    }

    #[test]
    fn unknown_tool_errors() {
        let r = call_tool(None, "heliosdb_missing", json!({}));
        assert!(r.is_error);
    }

    #[test]
    fn hybrid_search_without_index_errors() {
        let r = call_tool(
            None,
            "heliosdb_hybrid_search",
            json!({ "index_name": "nope", "query_text": "x" }),
        );
        assert!(r.is_error);
    }

    #[test]
    fn db_tool_with_db_works() {
        let db = EmbeddedDatabase::new_in_memory().expect("db");
        db.execute("CREATE TABLE t (id INT4 PRIMARY KEY, name TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'alpha')").unwrap();
        let r = call_tool(
            Some(&db),
            "heliosdb_query",
            json!({ "sql": "SELECT id, name FROM t" }),
        );
        assert!(!r.is_error, "{:?}", r.payload);
        assert_eq!(r.payload["row_count"].as_u64(), Some(1));
    }
}
