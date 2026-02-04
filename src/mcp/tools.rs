//! MCP Tool definitions for HeliosDB
//!
//! Provides the tool implementations that Claude can use to interact with the database.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::EmbeddedDatabase;
use super::protocol::{Tool, ToolResult};

/// Get all available tools
pub fn get_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "heliosdb_query".to_string(),
            description: "Execute a SQL query on the database. Returns query results as JSON.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "The SQL query to execute"
                    },
                    "params": {
                        "type": "array",
                        "items": {},
                        "description": "Query parameters for parameterized queries",
                        "default": []
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch to query (default: main)",
                        "default": "main"
                    }
                },
                "required": ["sql"]
            }),
        },
        Tool {
            name: "heliosdb_schema".to_string(),
            description: "Get the schema of a table including columns, types, and indexes.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "Name of the table to get schema for"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch to query (default: main)",
                        "default": "main"
                    }
                },
                "required": ["table"]
            }),
        },
        Tool {
            name: "heliosdb_list_tables".to_string(),
            description: "List all tables in the database.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "branch": {
                        "type": "string",
                        "description": "Branch to list tables from (default: main)",
                        "default": "main"
                    }
                }
            }),
        },
        Tool {
            name: "heliosdb_create_table".to_string(),
            description: "Create a new table in the database.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the table to create"
                    },
                    "columns": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "type": { "type": "string" },
                                "nullable": { "type": "boolean", "default": true },
                                "primary_key": { "type": "boolean", "default": false }
                            },
                            "required": ["name", "type"]
                        },
                        "description": "Column definitions"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch to create table on (default: main)",
                        "default": "main"
                    }
                },
                "required": ["name", "columns"]
            }),
        },
        Tool {
            name: "heliosdb_insert".to_string(),
            description: "Insert rows into a table.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "Name of the table to insert into"
                    },
                    "rows": {
                        "type": "array",
                        "items": {
                            "type": "object"
                        },
                        "description": "Array of row objects to insert"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch to insert into (default: main)",
                        "default": "main"
                    }
                },
                "required": ["table", "rows"]
            }),
        },
        Tool {
            name: "heliosdb_branch_create".to_string(),
            description: "Create a new branch from an existing branch.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name for the new branch"
                    },
                    "from_branch": {
                        "type": "string",
                        "description": "Branch to create from (default: main)",
                        "default": "main"
                    }
                },
                "required": ["name"]
            }),
        },
        Tool {
            name: "heliosdb_branch_list".to_string(),
            description: "List all branches in the database.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "heliosdb_branch_merge".to_string(),
            description: "Merge a branch into another branch.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "Source branch to merge from"
                    },
                    "target": {
                        "type": "string",
                        "description": "Target branch to merge into (default: main)",
                        "default": "main"
                    }
                },
                "required": ["source"]
            }),
        },
        Tool {
            name: "heliosdb_search".to_string(),
            description: "Perform semantic vector search on a table with vector embeddings.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "Table to search in"
                    },
                    "vector": {
                        "type": "array",
                        "items": { "type": "number" },
                        "description": "Query vector for similarity search"
                    },
                    "vector_column": {
                        "type": "string",
                        "description": "Column containing vectors",
                        "default": "embedding"
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Number of results to return",
                        "default": 10
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch to search in (default: main)",
                        "default": "main"
                    }
                },
                "required": ["table", "vector"]
            }),
        },
        Tool {
            name: "heliosdb_time_travel".to_string(),
            description: "Query the database at a specific point in time.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "SQL query to execute"
                    },
                    "timestamp": {
                        "type": "string",
                        "description": "ISO 8601 timestamp to query at"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch to query (default: main)",
                        "default": "main"
                    }
                },
                "required": ["sql", "timestamp"]
            }),
        },
    ]
}

/// Tool input types
#[derive(Debug, Deserialize)]
pub struct QueryInput {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<serde_json::Value>,
    #[serde(default = "default_branch")]
    pub branch: String,
}

#[derive(Debug, Deserialize)]
pub struct SchemaInput {
    pub table: String,
    #[serde(default = "default_branch")]
    pub branch: String,
}

#[derive(Debug, Deserialize)]
pub struct ListTablesInput {
    #[serde(default = "default_branch")]
    pub branch: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateTableInput {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    #[serde(default = "default_branch")]
    pub branch: String,
}

#[derive(Debug, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    #[serde(rename = "type")]
    pub col_type: String,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub primary_key: bool,
}

#[derive(Debug, Deserialize)]
pub struct InsertInput {
    pub table: String,
    pub rows: Vec<serde_json::Map<String, serde_json::Value>>,
    #[serde(default = "default_branch")]
    pub branch: String,
}

#[derive(Debug, Deserialize)]
pub struct BranchCreateInput {
    pub name: String,
    #[serde(default = "default_branch")]
    pub from_branch: String,
}

#[derive(Debug, Deserialize)]
pub struct BranchMergeInput {
    pub source: String,
    #[serde(default = "default_branch")]
    pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchInput {
    pub table: String,
    pub vector: Vec<f32>,
    #[serde(default = "default_vector_column")]
    pub vector_column: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default = "default_branch")]
    pub branch: String,
}

#[derive(Debug, Deserialize)]
pub struct TimeTravelInput {
    pub sql: String,
    pub timestamp: String,
    #[serde(default = "default_branch")]
    pub branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_vector_column() -> String {
    "embedding".to_string()
}

fn default_top_k() -> usize {
    10
}

/// Execute a tool with the given name and arguments
pub async fn execute_tool(
    db: Arc<EmbeddedDatabase>,
    tool_name: &str,
    args: serde_json::Value,
) -> ToolResult {
    match tool_name {
        "heliosdb_query" => execute_query(db, args).await,
        "heliosdb_schema" => execute_schema(db, args).await,
        "heliosdb_list_tables" => execute_list_tables(db, args).await,
        "heliosdb_create_table" => execute_create_table(db, args).await,
        "heliosdb_insert" => execute_insert(db, args).await,
        "heliosdb_branch_create" => execute_branch_create(db, args).await,
        "heliosdb_branch_list" => execute_branch_list(db, args).await,
        "heliosdb_branch_merge" => execute_branch_merge(db, args).await,
        "heliosdb_search" => execute_search(db, args).await,
        "heliosdb_time_travel" => execute_time_travel(db, args).await,
        _ => ToolResult::error(format!("Unknown tool: {}", tool_name)),
    }
}

async fn execute_query(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: QueryInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    // Convert JSON params to internal format
    let params: Vec<crate::Value> = input.params.iter().map(json_to_value).collect();

    match db.query_branch(&input.branch, &input.sql, params) {
        Ok(result) => {
            let output = json!({
                "columns": result.columns.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                "rows": result.rows,
                "row_count": result.rows.len()
            });
            ToolResult::json(&output)
        }
        Err(e) => ToolResult::error(format!("Query failed: {}", e)),
    }
}

async fn execute_schema(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: SchemaInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    // Get table info via query
    let sql = format!(
        "SELECT column_name, data_type, is_nullable FROM information_schema.columns WHERE table_name = '{}'",
        input.table.replace('\'', "''")
    );

    match db.query_branch(&input.branch, &sql, vec![]) {
        Ok(result) => {
            let output = json!({
                "table": input.table,
                "columns": result.rows
            });
            ToolResult::json(&output)
        }
        Err(e) => ToolResult::error(format!("Failed to get schema: {}", e)),
    }
}

async fn execute_list_tables(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: ListTablesInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    let sql = "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'";

    match db.query_branch(&input.branch, sql, vec![]) {
        Ok(result) => {
            let tables: Vec<String> = result.rows.iter()
                .filter_map(|row| row.first())
                .filter_map(|v| {
                    if let crate::Value::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect();
            ToolResult::json(&json!({ "tables": tables }))
        }
        Err(e) => ToolResult::error(format!("Failed to list tables: {}", e)),
    }
}

async fn execute_create_table(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: CreateTableInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    let columns: Vec<String> = input.columns.iter().map(|col| {
        let mut def = format!("{} {}", col.name, col.col_type);
        if !col.nullable {
            def.push_str(" NOT NULL");
        }
        if col.primary_key {
            def.push_str(" PRIMARY KEY");
        }
        def
    }).collect();

    let sql = format!("CREATE TABLE {} ({})", input.name, columns.join(", "));

    match db.execute_branch(&input.branch, &sql, vec![]) {
        Ok(_) => ToolResult::text(format!("Table '{}' created successfully", input.name)),
        Err(e) => ToolResult::error(format!("Failed to create table: {}", e)),
    }
}

async fn execute_insert(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: InsertInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    if input.rows.is_empty() {
        return ToolResult::error("No rows to insert".to_string());
    }

    let mut total_inserted = 0;

    for row in &input.rows {
        let columns: Vec<&str> = row.keys().map(|s| s.as_str()).collect();
        let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("${}", i)).collect();
        let values: Vec<crate::Value> = row.values().map(json_to_value).collect();

        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            input.table,
            columns.join(", "),
            placeholders.join(", ")
        );

        match db.execute_branch(&input.branch, &sql, values) {
            Ok(_) => total_inserted += 1,
            Err(e) => return ToolResult::error(format!("Insert failed: {}", e)),
        }
    }

    ToolResult::text(format!("Inserted {} row(s) into '{}'", total_inserted, input.table))
}

async fn execute_branch_create(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: BranchCreateInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    match db.create_branch(&input.name, &input.from_branch) {
        Ok(_) => ToolResult::text(format!(
            "Branch '{}' created from '{}'",
            input.name, input.from_branch
        )),
        Err(e) => ToolResult::error(format!("Failed to create branch: {}", e)),
    }
}

async fn execute_branch_list(db: Arc<EmbeddedDatabase>, _args: serde_json::Value) -> ToolResult {
    match db.list_branches() {
        Ok(branches) => ToolResult::json(&json!({ "branches": branches })),
        Err(e) => ToolResult::error(format!("Failed to list branches: {}", e)),
    }
}

async fn execute_branch_merge(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: BranchMergeInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    match db.merge_branches(&input.source, &input.target) {
        Ok(_) => ToolResult::text(format!(
            "Branch '{}' merged into '{}'",
            input.source, input.target
        )),
        Err(e) => ToolResult::error(format!("Merge failed: {}", e)),
    }
}

async fn execute_search(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: SearchInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    // Vector search using SQL extension
    let sql = format!(
        "SELECT *, vector_distance({}, $1) as distance FROM {} ORDER BY distance LIMIT {}",
        input.vector_column, input.table, input.top_k
    );

    let vector_value = crate::Value::Vector(input.vector);

    match db.query_branch(&input.branch, &sql, vec![vector_value]) {
        Ok(result) => ToolResult::json(&json!({
            "results": result.rows,
            "count": result.rows.len()
        })),
        Err(e) => ToolResult::error(format!("Search failed: {}", e)),
    }
}

async fn execute_time_travel(db: Arc<EmbeddedDatabase>, args: serde_json::Value) -> ToolResult {
    let input: TimeTravelInput = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Invalid arguments: {}", e)),
    };

    // Parse timestamp
    let timestamp = match chrono::DateTime::parse_from_rfc3339(&input.timestamp) {
        Ok(t) => t.timestamp() as u64,
        Err(e) => return ToolResult::error(format!("Invalid timestamp: {}", e)),
    };

    match db.query_at_timestamp(&input.branch, &input.sql, vec![], timestamp) {
        Ok(result) => {
            let output = json!({
                "columns": result.columns.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                "rows": result.rows,
                "row_count": result.rows.len(),
                "timestamp": input.timestamp
            });
            ToolResult::json(&output)
        }
        Err(e) => ToolResult::error(format!("Time travel query failed: {}", e)),
    }
}

/// Convert JSON value to internal Value type
fn json_to_value(v: &serde_json::Value) -> crate::Value {
    match v {
        serde_json::Value::Null => crate::Value::Null,
        serde_json::Value::Bool(b) => crate::Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                crate::Value::Int8(i)
            } else if let Some(f) = n.as_f64() {
                crate::Value::Float8(f)
            } else {
                crate::Value::Null
            }
        }
        serde_json::Value::String(s) => crate::Value::String(s.clone()),
        serde_json::Value::Array(arr) => {
            // Try to parse as vector
            let floats: Result<Vec<f32>, _> = arr.iter()
                .map(|v| v.as_f64().map(|f| f as f32).ok_or(()))
                .collect();
            if let Ok(vec) = floats {
                crate::Value::Vector(vec)
            } else {
                crate::Value::String(serde_json::to_string(arr).unwrap_or_default())
            }
        }
        serde_json::Value::Object(_) => {
            crate::Value::String(serde_json::to_string(v).unwrap_or_default())
        }
    }
}
