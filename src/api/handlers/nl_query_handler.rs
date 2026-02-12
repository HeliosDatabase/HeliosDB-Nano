//! Natural Language Query API handlers
//!
//! Provides REST API endpoints for converting natural language to SQL:
//! - POST /v1/nl/query - Convert NL to SQL
//! - POST /v1/nl/execute - Convert NL to SQL and execute
//! - POST /v1/nl/explain - Explain a natural language query
//! - GET /v1/nl/schema - Get schema context for NL queries
//! - POST /v1/nl/suggest - Get query suggestions

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, error};

use crate::api::models::ApiError;
use crate::api::server::AppState;
use crate::storage::dump::DatabaseInterface;
use crate::ai::nl_query::{
    NlQueryEngine, NlQueryRequest, NlQueryConfig,
    SchemaContext, TableSchema, ColumnSchema, ConversationContext,
    QueryHistoryEntry, SqlDialect,
};

// ============================================================================
// Request/Response Types
// ============================================================================

/// NL Query request body
#[derive(Debug, Deserialize)]
pub struct NlQueryApiRequest {
    /// Natural language question
    pub question: String,
    /// Database/schema to query (optional, uses default)
    pub database: Option<String>,
    /// Schema name (optional)
    pub schema: Option<String>,
    /// Specific tables to include (optional, uses all visible)
    pub tables: Option<Vec<String>>,
    /// Previous conversation context
    pub context: Option<Vec<ConversationEntry>>,
    /// Session ID for conversation tracking
    pub session_id: Option<String>,
    /// Configuration overrides
    pub config: Option<NlQueryConfigOverride>,
}

/// Conversation entry for context
#[derive(Debug, Deserialize)]
pub struct ConversationEntry {
    pub question: String,
    pub sql: String,
    pub success: bool,
}

/// Configuration overrides
#[derive(Debug, Deserialize)]
pub struct NlQueryConfigOverride {
    /// SQL dialect
    pub dialect: Option<String>,
    /// Maximum results
    pub max_results: Option<usize>,
    /// Whether to validate SQL
    pub validate_sql: Option<bool>,
    /// Whether to explain results
    pub explain_results: Option<bool>,
    /// LLM temperature
    pub temperature: Option<f32>,
    /// Model to use
    pub model: Option<String>,
}

/// NL Query API response
#[derive(Debug, Serialize)]
pub struct NlQueryApiResponse {
    /// Generated SQL query
    pub sql: String,
    /// Query explanation
    pub explanation: Option<String>,
    /// Confidence score (0.0 - 1.0)
    pub confidence: f32,
    /// Detected intent
    pub intent: String,
    /// Tables referenced
    pub tables: Vec<String>,
    /// Whether query is valid
    pub valid: bool,
    /// Validation errors (if any)
    pub validation_errors: Vec<String>,
    /// Warnings
    pub warnings: Vec<String>,
    /// Suggested queries
    pub suggestions: Vec<SuggestionResponse>,
    /// Processing time in ms
    pub processing_time_ms: u64,
    /// Whether result was cached
    pub cached: bool,
}

/// Suggestion response
#[derive(Debug, Serialize)]
pub struct SuggestionResponse {
    pub text: String,
    pub sql: Option<String>,
    pub reason: String,
}

/// NL Execute request (query + execute)
#[derive(Debug, Deserialize)]
pub struct NlExecuteRequest {
    /// Natural language question
    pub question: String,
    /// Branch to execute on
    pub branch: Option<String>,
    /// Database/schema context
    pub database: Option<String>,
    /// Schema name
    pub schema: Option<String>,
    /// Tables to include
    pub tables: Option<Vec<String>>,
    /// Maximum rows to return
    pub limit: Option<usize>,
    /// Conversation context
    pub context: Option<Vec<ConversationEntry>>,
    /// Session ID
    pub session_id: Option<String>,
    /// Configuration
    pub config: Option<NlQueryConfigOverride>,
}

/// NL Execute response
#[derive(Debug, Serialize)]
pub struct NlExecuteResponse {
    /// Generated SQL
    pub sql: String,
    /// Query explanation
    pub explanation: Option<String>,
    /// Confidence score
    pub confidence: f32,
    /// Column names
    pub columns: Vec<String>,
    /// Column types
    pub column_types: Vec<String>,
    /// Result rows
    pub rows: Vec<HashMap<String, serde_json::Value>>,
    /// Row count
    pub row_count: usize,
    /// NL processing time
    pub nl_processing_time_ms: u64,
    /// SQL execution time
    pub sql_execution_time_ms: u64,
    /// Total time
    pub total_time_ms: u64,
    /// Warnings
    pub warnings: Vec<String>,
}

/// Schema context response
#[derive(Debug, Serialize)]
pub struct SchemaContextResponse {
    /// Available tables
    pub tables: Vec<TableSchemaResponse>,
    /// Database name
    pub database: Option<String>,
    /// Schema name
    pub schema: Option<String>,
}

/// Table schema response
#[derive(Debug, Serialize)]
pub struct TableSchemaResponse {
    pub name: String,
    pub description: Option<String>,
    pub columns: Vec<ColumnSchemaResponse>,
    pub primary_key: Option<Vec<String>>,
    pub foreign_keys: Option<Vec<ForeignKeyResponse>>,
    pub row_count: Option<usize>,
}

/// Column schema response
#[derive(Debug, Serialize)]
pub struct ColumnSchemaResponse {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub description: Option<String>,
    pub is_primary_key: bool,
}

/// Foreign key response
#[derive(Debug, Serialize)]
pub struct ForeignKeyResponse {
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// Explain request
#[derive(Debug, Deserialize)]
pub struct NlExplainRequest {
    /// SQL query to explain
    pub sql: String,
    /// Natural language question (optional, for context)
    pub question: Option<String>,
}

/// Explain response
#[derive(Debug, Serialize)]
pub struct NlExplainResponse {
    /// Natural language explanation
    pub explanation: String,
    /// Query breakdown
    pub breakdown: QueryBreakdown,
    /// Suggestions for improvement
    pub suggestions: Vec<String>,
}

/// Query breakdown
#[derive(Debug, Serialize)]
pub struct QueryBreakdown {
    /// Operation type
    pub operation: String,
    /// Tables involved
    pub tables: Vec<String>,
    /// Columns selected
    pub columns: Vec<String>,
    /// Conditions
    pub conditions: Vec<String>,
    /// Joins
    pub joins: Vec<String>,
    /// Aggregations
    pub aggregations: Vec<String>,
    /// Order by
    pub order_by: Option<String>,
    /// Limit
    pub limit: Option<usize>,
}

/// Suggestion request
#[derive(Debug, Deserialize)]
pub struct NlSuggestRequest {
    /// Partial question or context
    pub partial: String,
    /// Database context
    pub database: Option<String>,
    /// Schema context
    pub schema: Option<String>,
    /// Maximum suggestions
    pub limit: Option<usize>,
}

/// Suggestion response list
#[derive(Debug, Serialize)]
pub struct NlSuggestResponse {
    pub suggestions: Vec<QuerySuggestionResponse>,
}

/// Single suggestion
#[derive(Debug, Serialize)]
pub struct QuerySuggestionResponse {
    /// Suggested question
    pub question: String,
    /// Category
    pub category: String,
    /// Complexity
    pub complexity: String,
}

/// Query parameters for schema endpoint
#[derive(Debug, Deserialize)]
pub struct SchemaQueryParams {
    /// Database name
    pub database: Option<String>,
    /// Schema name
    pub schema: Option<String>,
    /// Include only specific tables
    pub tables: Option<String>,
    /// Include sample values
    pub include_samples: Option<bool>,
}

// ============================================================================
// Handler Implementations
// ============================================================================

/// Convert natural language to SQL
///
/// POST /v1/nl/query
pub async fn nl_to_sql(
    State(state): State<AppState>,
    Json(request): Json<NlQueryApiRequest>,
) -> Result<Json<NlQueryApiResponse>, ApiError> {
    let start = Instant::now();
    info!("NL Query request: {}", request.question);

    // Build schema context
    let schema_context = build_schema_context(&state, &request).await?;

    // Build conversation context
    let conversation_context = request.context.map(|entries| {
        ConversationContext {
            history: entries.into_iter().map(|e| QueryHistoryEntry {
                question: e.question,
                sql: e.sql,
                success: e.success,
                timestamp: None,
            }).collect(),
            entities: None,
            session_id: request.session_id.clone(),
        }
    });

    // Build config
    let config = build_config(request.config);

    // Create NL query request
    let nl_request = NlQueryRequest {
        question: request.question.clone(),
        schema: Some(schema_context),
        context: conversation_context,
        config: Some(config),
        user_id: None, // Would come from auth
        tenant_id: None, // Would come from tenant context
        metadata: None,
    };

    // Get or create NL query engine
    let engine = get_nl_engine(&state)?;

    // Translate
    let response = engine.translate(nl_request).await
        .map_err(|e| ApiError::internal(format!("NL translation failed: {}", e)))?;

    // Build API response
    let api_response = NlQueryApiResponse {
        sql: response.sql,
        explanation: response.explanation,
        confidence: response.confidence,
        intent: format!("{:?}", response.analysis.intent).to_lowercase(),
        tables: response.analysis.tables,
        valid: response.validation.as_ref().map(|v| v.allowed).unwrap_or(true),
        validation_errors: response.validation
            .as_ref()
            .map(|v| v.errors.iter().map(|e| e.message.clone()).collect())
            .unwrap_or_default(),
        warnings: response.warnings,
        suggestions: response.suggestions
            .unwrap_or_default()
            .into_iter()
            .map(|s| SuggestionResponse {
                text: s.text,
                sql: s.sql,
                reason: s.reason,
            })
            .collect(),
        processing_time_ms: start.elapsed().as_millis() as u64,
        cached: response.cached,
    };

    info!(
        "NL Query completed in {}ms, confidence: {:.2}",
        api_response.processing_time_ms,
        api_response.confidence
    );

    Ok(Json(api_response))
}

/// Convert natural language to SQL and execute
///
/// POST /v1/nl/execute
pub async fn nl_execute(
    State(state): State<AppState>,
    Json(request): Json<NlExecuteRequest>,
) -> Result<Json<NlExecuteResponse>, ApiError> {
    let total_start = Instant::now();
    info!("NL Execute request: {}", request.question);

    // First, translate NL to SQL
    let nl_start = Instant::now();

    let schema_context = build_schema_context_from_execute(&state, &request).await?;
    let conversation_context = request.context.map(|entries| {
        ConversationContext {
            history: entries.into_iter().map(|e| QueryHistoryEntry {
                question: e.question,
                sql: e.sql,
                success: e.success,
                timestamp: None,
            }).collect(),
            entities: None,
            session_id: request.session_id.clone(),
        }
    });

    let config = build_config(request.config);

    let nl_request = NlQueryRequest {
        question: request.question.clone(),
        schema: Some(schema_context),
        context: conversation_context,
        config: Some(config.clone()),
        user_id: None,
        tenant_id: None,
        metadata: None,
    };

    let engine = get_nl_engine(&state)?;
    let nl_response = engine.translate(nl_request).await
        .map_err(|e| ApiError::internal(format!("NL translation failed: {}", e)))?;

    let nl_processing_time = nl_start.elapsed().as_millis() as u64;

    // Check if valid
    if let Some(ref v) = nl_response.validation {
        if !v.allowed {
            return Err(ApiError::bad_request(format!(
                "Generated SQL is not valid: {}",
                v.errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
    }

    // Execute the SQL
    let sql_start = Instant::now();
    let branch = request.branch.as_deref().unwrap_or("main");

    // Add limit if not present
    let sql = if request.limit.is_some() && !nl_response.sql.to_uppercase().contains("LIMIT") {
        format!("{} LIMIT {}", nl_response.sql, request.limit.unwrap_or(1000))
    } else {
        nl_response.sql.clone()
    };

    // Execute query
    let tuples = state.db.query(&sql, &[])
        .map_err(|e| {
            error!("SQL execution failed: {}", e);
            ApiError::from(e)
        })?;

    let sql_execution_time = sql_start.elapsed().as_millis() as u64;

    // Build response
    let (columns, column_types, rows) = if tuples.is_empty() {
        (vec![], vec![], vec![])
    } else if let Some(first) = tuples.first() {
        let cols: Vec<String> = (0..first.values.len())
            .map(|i| format!("column_{}", i))
            .collect();
        let types: Vec<String> = first.values.iter()
            .map(|v| format!("{:?}", v).split('(').next().unwrap_or("unknown").to_lowercase())
            .collect();

        let rows: Vec<HashMap<String, serde_json::Value>> = tuples.iter().map(|t| {
            let mut row = HashMap::new();
            for (i, v) in t.values.iter().enumerate() {
                let json_val: serde_json::Value = v.into();
                row.insert(cols.get(i).cloned().unwrap_or_default(), json_val);
            }
            row
        }).collect();

        (cols, types, rows)
    } else {
        (vec![], vec![], vec![])
    };

    let total_time = total_start.elapsed().as_millis() as u64;

    let response = NlExecuteResponse {
        sql: nl_response.sql,
        explanation: nl_response.explanation,
        confidence: nl_response.confidence,
        columns,
        column_types,
        row_count: rows.len(),
        rows,
        nl_processing_time_ms: nl_processing_time,
        sql_execution_time_ms: sql_execution_time,
        total_time_ms: total_time,
        warnings: nl_response.warnings,
    };

    info!(
        "NL Execute completed: {} rows in {}ms (NL: {}ms, SQL: {}ms)",
        response.row_count,
        total_time,
        nl_processing_time,
        sql_execution_time
    );

    Ok(Json(response))
}

/// Explain a SQL query in natural language
///
/// POST /v1/nl/explain
pub async fn nl_explain(
    State(state): State<AppState>,
    Json(request): Json<NlExplainRequest>,
) -> Result<Json<NlExplainResponse>, ApiError> {
    info!("NL Explain request for SQL: {}", request.sql);

    let sql_upper = request.sql.to_uppercase();

    // Parse SQL to extract components (simplified)
    let operation = if sql_upper.starts_with("SELECT") {
        "SELECT"
    } else if sql_upper.starts_with("INSERT") {
        "INSERT"
    } else if sql_upper.starts_with("UPDATE") {
        "UPDATE"
    } else if sql_upper.starts_with("DELETE") {
        "DELETE"
    } else {
        "UNKNOWN"
    };

    // Extract tables (simplified regex-based extraction)
    let tables = extract_tables_from_sql(&request.sql);
    let conditions = extract_conditions_from_sql(&request.sql);
    let joins = extract_joins_from_sql(&request.sql);

    // Build explanation
    let mut explanation_parts = Vec::new();

    match operation {
        "SELECT" => {
            explanation_parts.push(format!(
                "This query retrieves data from {}.",
                if tables.is_empty() {
                    "the database".to_string()
                } else {
                    format!("the {} table(s)", tables.join(", "))
                }
            ));
        }
        "INSERT" => {
            explanation_parts.push("This query inserts new data.".to_string());
        }
        "UPDATE" => {
            explanation_parts.push("This query updates existing data.".to_string());
        }
        "DELETE" => {
            explanation_parts.push("This query deletes data.".to_string());
        }
        _ => {
            explanation_parts.push("This is a database operation.".to_string());
        }
    }

    if !conditions.is_empty() {
        explanation_parts.push(format!(
            "It filters results where {}.",
            conditions.join(" and ")
        ));
    }

    if !joins.is_empty() {
        explanation_parts.push(format!(
            "It combines data using {} join(s).",
            joins.len()
        ));
    }

    // Extract limit
    let limit = if let Some(pos) = sql_upper.find("LIMIT") {
        let after = &request.sql[pos + 5..];
        after.trim().split_whitespace().next()
            .and_then(|s| s.parse::<usize>().ok())
    } else {
        None
    };

    if let Some(lim) = limit {
        explanation_parts.push(format!("Results are limited to {} rows.", lim));
    }

    // Build suggestions
    let mut suggestions = Vec::new();
    if limit.is_none() && operation == "SELECT" {
        suggestions.push("Consider adding a LIMIT clause for large tables.".to_string());
    }
    if sql_upper.contains("SELECT *") {
        suggestions.push("Consider selecting specific columns instead of *.".to_string());
    }

    let response = NlExplainResponse {
        explanation: explanation_parts.join(" "),
        breakdown: QueryBreakdown {
            operation: operation.to_string(),
            tables,
            columns: extract_columns_from_sql(&request.sql),
            conditions,
            joins,
            aggregations: extract_aggregations_from_sql(&request.sql),
            order_by: extract_order_by_from_sql(&request.sql),
            limit,
        },
        suggestions,
    };

    Ok(Json(response))
}

/// Get schema context for NL queries
///
/// GET /v1/nl/schema
pub async fn get_schema_context(
    State(state): State<AppState>,
    Query(params): Query<SchemaQueryParams>,
) -> Result<Json<SchemaContextResponse>, ApiError> {
    info!("Getting schema context for NL queries");

    // Get tables from catalog
    let tables_result = state.db.list_tables()
        .map_err(|e| ApiError::internal(format!("Failed to list tables: {}", e)))?;

    let filter_tables: Option<Vec<String>> = params.tables
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect());

    let mut tables = Vec::new();
    for table_name in tables_result {
        // Filter if specific tables requested
        if let Some(ref filter) = filter_tables {
            if !filter.iter().any(|f| f.eq_ignore_ascii_case(&table_name)) {
                continue;
            }
        }

        // Get table schema
        if let Ok(schema) = state.db.get_table_schema(&table_name) {
            let columns: Vec<ColumnSchemaResponse> = schema.columns.iter().map(|c| {
                ColumnSchemaResponse {
                    name: c.name.clone(),
                    data_type: format!("{:?}", c.data_type),
                    nullable: c.nullable,
                    description: None,
                    is_primary_key: c.primary_key,
                }
            }).collect();

            let primary_key: Vec<String> = schema.columns.iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();

            tables.push(TableSchemaResponse {
                name: table_name,
                description: None,
                columns,
                primary_key: if primary_key.is_empty() { None } else { Some(primary_key) },
                foreign_keys: None, // Would need FK introspection
                row_count: None, // Would need count query
            });
        }
    }

    let response = SchemaContextResponse {
        tables,
        database: params.database,
        schema: params.schema,
    };

    Ok(Json(response))
}

/// Get query suggestions based on partial input
///
/// POST /v1/nl/suggest
pub async fn nl_suggest(
    State(state): State<AppState>,
    Json(request): Json<NlSuggestRequest>,
) -> Result<Json<NlSuggestResponse>, ApiError> {
    info!("NL Suggest request: {}", request.partial);

    let limit = request.limit.unwrap_or(5);
    let partial_lower = request.partial.to_lowercase();

    // Get table names for context-aware suggestions
    let tables = state.db.list_tables().unwrap_or_default();

    let mut suggestions = Vec::new();

    // Generate suggestions based on partial input
    if partial_lower.contains("how many") || partial_lower.contains("count") {
        for table in tables.iter().take(3) {
            suggestions.push(QuerySuggestionResponse {
                question: format!("How many records are in {}?", table),
                category: "count".to_string(),
                complexity: "simple".to_string(),
            });
        }
    }

    if partial_lower.contains("show") || partial_lower.contains("list") || partial_lower.contains("get") {
        for table in tables.iter().take(3) {
            suggestions.push(QuerySuggestionResponse {
                question: format!("Show all records from {}", table),
                category: "select".to_string(),
                complexity: "simple".to_string(),
            });
            suggestions.push(QuerySuggestionResponse {
                question: format!("Show the top 10 records from {}", table),
                category: "select".to_string(),
                complexity: "simple".to_string(),
            });
        }
    }

    if partial_lower.contains("average") || partial_lower.contains("avg") || partial_lower.contains("total") || partial_lower.contains("sum") {
        suggestions.push(QuerySuggestionResponse {
            question: "What is the average value?".to_string(),
            category: "aggregate".to_string(),
            complexity: "medium".to_string(),
        });
        suggestions.push(QuerySuggestionResponse {
            question: "What is the total sum?".to_string(),
            category: "aggregate".to_string(),
            complexity: "medium".to_string(),
        });
    }

    if partial_lower.contains("group") || partial_lower.contains("by") {
        suggestions.push(QuerySuggestionResponse {
            question: "Group records by category".to_string(),
            category: "group".to_string(),
            complexity: "medium".to_string(),
        });
    }

    // Add generic suggestions if not enough specific ones
    if suggestions.len() < limit {
        for table in tables.iter() {
            if suggestions.len() >= limit {
                break;
            }
            suggestions.push(QuerySuggestionResponse {
                question: format!("Find records in {} where...", table),
                category: "search".to_string(),
                complexity: "simple".to_string(),
            });
        }
    }

    // Limit results
    suggestions.truncate(limit);

    Ok(Json(NlSuggestResponse { suggestions }))
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Build schema context from API request
async fn build_schema_context(
    state: &AppState,
    request: &NlQueryApiRequest,
) -> Result<SchemaContext, ApiError> {
    let tables_list = if let Some(ref tables) = request.tables {
        tables.clone()
    } else {
        state.db.list_tables()
            .map_err(|e| ApiError::internal(format!("Failed to list tables: {}", e)))?
    };

    let mut tables = Vec::new();
    for table_name in tables_list {
        if let Ok(schema) = state.db.get_table_schema(&table_name) {
            let columns: Vec<ColumnSchema> = schema.columns.iter().map(|c| {
                ColumnSchema {
                    name: c.name.clone(),
                    data_type: format!("{:?}", c.data_type),
                    nullable: c.nullable,
                    description: None,
                    default_value: None,
                    is_primary_key: c.primary_key,
                    is_unique: false,
                    enum_values: None,
                }
            }).collect();

            let primary_key: Vec<String> = schema.columns.iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();

            tables.push(TableSchema {
                name: table_name,
                description: None,
                columns,
                primary_key: if primary_key.is_empty() { None } else { Some(primary_key) },
                foreign_keys: None,
                indexes: None,
                sample_values: None,
                row_count: None,
            });
        }
    }

    Ok(SchemaContext {
        tables,
        database: request.database.clone(),
        schema: request.schema.clone(),
        hints: None,
    })
}

/// Build schema context from execute request
async fn build_schema_context_from_execute(
    state: &AppState,
    request: &NlExecuteRequest,
) -> Result<SchemaContext, ApiError> {
    let tables_list = if let Some(ref tables) = request.tables {
        tables.clone()
    } else {
        state.db.list_tables()
            .map_err(|e| ApiError::internal(format!("Failed to list tables: {}", e)))?
    };

    let mut tables = Vec::new();
    for table_name in tables_list {
        if let Ok(schema) = state.db.get_table_schema(&table_name) {
            let columns: Vec<ColumnSchema> = schema.columns.iter().map(|c| {
                ColumnSchema {
                    name: c.name.clone(),
                    data_type: format!("{:?}", c.data_type),
                    nullable: c.nullable,
                    description: None,
                    default_value: None,
                    is_primary_key: c.primary_key,
                    is_unique: false,
                    enum_values: None,
                }
            }).collect();

            let primary_key: Vec<String> = schema.columns.iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();

            tables.push(TableSchema {
                name: table_name,
                description: None,
                columns,
                primary_key: if primary_key.is_empty() { None } else { Some(primary_key) },
                foreign_keys: None,
                indexes: None,
                sample_values: None,
                row_count: None,
            });
        }
    }

    Ok(SchemaContext {
        tables,
        database: request.database.clone(),
        schema: request.schema.clone(),
        hints: None,
    })
}

/// Build NL query config from overrides
fn build_config(overrides: Option<NlQueryConfigOverride>) -> NlQueryConfig {
    let mut config = NlQueryConfig::default();

    if let Some(o) = overrides {
        if let Some(dialect) = o.dialect {
            config.dialect = match dialect.to_lowercase().as_str() {
                "postgresql" | "postgres" => SqlDialect::PostgreSQL,
                "mysql" => SqlDialect::MySQL,
                "sqlite" => SqlDialect::SQLite,
                "mssql" | "sqlserver" => SqlDialect::MSSQL,
                "oracle" => SqlDialect::Oracle,
                "heliosdb" => SqlDialect::HeliosDB,
                _ => SqlDialect::PostgreSQL,
            };
        }
        if let Some(max) = o.max_results {
            config.max_results = max;
        }
        if let Some(validate) = o.validate_sql {
            config.validate_sql = validate;
        }
        if let Some(explain) = o.explain_results {
            config.explain_results = explain;
        }
        if let Some(temp) = o.temperature {
            config.temperature = temp;
        }
        if let Some(model) = o.model {
            config.model = Some(model);
        }
    }

    config
}

/// Get or create NL query engine
fn get_nl_engine(state: &AppState) -> Result<Arc<NlQueryEngine>, ApiError> {
    // In a real implementation, this would be cached in AppState
    // For now, we create a mock provider
    use crate::ai::providers::{LlmProviderConfig, ProviderRegistry};

    // Try to get configured provider
    let provider_config = LlmProviderConfig {
        provider: "ollama".to_string(), // Default to Ollama for local
        api_key: None,
        endpoint: Some("http://localhost:11434".to_string()),
        model: Some("llama3.2".to_string()),
        organization: None,
        deployment: None,
        api_version: None,
        timeout_ms: Some(30000),
        max_retries: Some(3),
        headers: None,
    };

    let provider = ProviderRegistry::from_config(&provider_config)
        .map_err(|e| ApiError::internal(format!("Failed to create LLM provider: {}", e)))?;

    Ok(Arc::new(NlQueryEngine::new(provider)))
}

/// Extract tables from SQL
fn extract_tables_from_sql(sql: &str) -> Vec<String> {
    let mut tables = Vec::new();
    let re = regex::Regex::new(r"(?i)\b(?:FROM|JOIN|INTO|UPDATE)\s+([a-zA-Z_][a-zA-Z0-9_]*)").ok();

    if let Some(re) = re {
        for cap in re.captures_iter(sql) {
            if let Some(m) = cap.get(1) {
                let table = m.as_str().to_string();
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
        }
    }

    tables
}

/// Extract conditions from SQL
fn extract_conditions_from_sql(sql: &str) -> Vec<String> {
    let mut conditions = Vec::new();

    if let Some(where_pos) = sql.to_uppercase().find("WHERE") {
        let after = &sql[where_pos + 5..];
        // Find end (ORDER BY, GROUP BY, LIMIT, or end)
        let end = after.to_uppercase()
            .find("ORDER BY")
            .or_else(|| after.to_uppercase().find("GROUP BY"))
            .or_else(|| after.to_uppercase().find("LIMIT"))
            .unwrap_or(after.len());

        let where_clause = after[..end].trim();
        // Split by AND/OR
        for part in where_clause.split(['(', ')']) {
            let trimmed = part.trim();
            if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("AND") && !trimmed.eq_ignore_ascii_case("OR") {
                conditions.push(trimmed.to_string());
            }
        }
    }

    conditions
}

/// Extract joins from SQL
fn extract_joins_from_sql(sql: &str) -> Vec<String> {
    let mut joins = Vec::new();
    let re = regex::Regex::new(r"(?i)((?:LEFT|RIGHT|INNER|OUTER|CROSS|FULL)?\s*JOIN\s+[^\s]+\s+(?:ON\s+[^,]+)?)").ok();

    if let Some(re) = re {
        for cap in re.captures_iter(sql) {
            if let Some(m) = cap.get(1) {
                joins.push(m.as_str().trim().to_string());
            }
        }
    }

    joins
}

/// Extract columns from SQL
fn extract_columns_from_sql(sql: &str) -> Vec<String> {
    let upper = sql.to_uppercase();
    if let Some(select_pos) = upper.find("SELECT") {
        if let Some(from_pos) = upper.find("FROM") {
            let columns_part = &sql[select_pos + 6..from_pos];
            return columns_part.split(',')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();
        }
    }
    Vec::new()
}

/// Extract aggregations from SQL
fn extract_aggregations_from_sql(sql: &str) -> Vec<String> {
    let mut aggs = Vec::new();
    let re = regex::Regex::new(r"(?i)(COUNT|SUM|AVG|MIN|MAX)\s*\([^)]+\)").ok();

    if let Some(re) = re {
        for cap in re.captures_iter(sql) {
            if let Some(m) = cap.get(0) {
                aggs.push(m.as_str().to_string());
            }
        }
    }

    aggs
}

/// Extract ORDER BY from SQL
fn extract_order_by_from_sql(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    if let Some(pos) = upper.find("ORDER BY") {
        let after = &sql[pos + 8..];
        let end = after.to_uppercase()
            .find("LIMIT")
            .unwrap_or(after.len());
        return Some(after[..end].trim().to_string());
    }
    None
}
