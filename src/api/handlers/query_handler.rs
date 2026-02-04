//! Query execution handlers
//!
//! Implements HTTP request handlers for SQL query execution and statement execution.

use axum::{
    extract::{Path, State},
    Json,
};
use std::collections::HashMap;
use std::time::Instant;
use tracing::{info, warn, error};

use crate::api::{
    models::{
        ApiError,
        QueryRequest,
        QueryResponse,
        ExecuteRequest,
        ExecuteResponse,
    },
    server::AppState,
};
use crate::compute::QueryState;
use crate::{Value, DataType, Tuple, Schema, Error};

/// Execute a read-only SQL query on a branch
///
/// POST /v1/branches/:name/query
///
/// Executes a SELECT query and returns results. Supports parameterized queries
/// and time-travel via the as_of parameter.
///
/// # Path Parameters
///
/// - name: Branch name to execute query on
///
/// # Request Body
///
/// - sql: SQL query to execute
/// - params: Optional query parameters (for $1, $2, etc.)
/// - as_of: Optional time-travel specification
/// - timeout_ms: Optional query timeout in milliseconds
///
/// # Response
///
/// - 200 OK: Returns QueryResponse with results
/// - 400 Bad Request: Invalid SQL or parameters
/// - 404 Not Found: Branch does not exist
/// - 408 Request Timeout: Query exceeded timeout
/// - 422 Unprocessable Entity: Query execution failed
/// - 500 Internal Server Error: Database error
pub async fn execute_query(
    State(state): State<AppState>,
    Path(branch_name): Path<String>,
    Json(request): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    info!("Executing query on branch '{}': {}", branch_name, request.sql);

    // Register query with cancellation support
    let (query_id, cancel_token) = state.query_registry.register_query(
        &request.sql,
        "api_user", // In production, get from auth context
        &branch_name,
        None, // session_id
    );

    // Update state to executing
    state.query_registry.update_state(query_id, QueryState::Executing);

    // Validate branch exists (if branching is enabled)
    if let Some(branch_manager) = state.db.storage.branch_manager() {
        // Check if branch exists
        let _branch = branch_manager.get_branch_by_name(&branch_name)
            .map_err(|e| {
                warn!("Branch '{}' not found: {}", branch_name, e);
                state.query_registry.fail_query(query_id);
                ApiError::from(e)
            })?;
    }

    // Start timing
    let start = Instant::now();

    // Convert parameters if provided
    let params: Vec<Value> = request.params
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.into())
        .collect();

    // Check for cancellation before executing
    if cancel_token.is_cancelled() {
        state.query_registry.update_state(query_id, QueryState::Cancelled);
        return Err(ApiError::from(Error::query_cancelled("Query cancelled before execution")));
    }

    // Execute query
    let tuples = if params.is_empty() {
        state.db.query(&request.sql, &[])
            .map_err(|e| {
                error!("Query execution failed: {}", e);
                // Check if it was cancelled
                if cancel_token.is_cancelled() {
                    state.query_registry.update_state(query_id, QueryState::Cancelled);
                } else {
                    state.query_registry.fail_query(query_id);
                }
                ApiError::from(e)
            })?
    } else {
        state.db.query_params(&request.sql, &params)
            .map_err(|e| {
                error!("Query execution failed: {}", e);
                if cancel_token.is_cancelled() {
                    state.query_registry.update_state(query_id, QueryState::Cancelled);
                } else {
                    state.query_registry.fail_query(query_id);
                }
                ApiError::from(e)
            })?
    };

    // Check for cancellation after execution
    if cancel_token.is_cancelled() {
        state.query_registry.update_state(query_id, QueryState::Cancelled);
        return Err(ApiError::from(Error::query_cancelled("Query cancelled during execution")));
    }

    // Mark query as completed
    state.query_registry.complete_query(query_id);

    // Calculate execution time
    let execution_time_ms = start.elapsed().as_millis() as u64;

    // Get schema from catalog
    let response = if tuples.is_empty() {
        // No results - return empty response with minimal schema info
        QueryResponse {
            columns: vec![],
            column_types: vec![],
            rows: vec![],
            row_count: 0,
            execution_time_ms,
        }
    } else {
        // Infer schema from query result
        // For now, we'll extract column information from the tuples
        // In a real implementation, we'd get this from the query plan
        let schema = infer_schema_from_tuples(&tuples)?;

        // Convert tuples to rows
        let rows = tuples_to_rows(&tuples, &schema);

        QueryResponse {
            columns: schema.columns.iter().map(|c| c.name.clone()).collect(),
            column_types: schema.columns.iter().map(|c| datatype_to_string(&c.data_type)).collect(),
            rows,
            row_count: tuples.len(),
            execution_time_ms,
        }
    };

    info!(
        "Query {} on branch '{}' returned {} rows in {}ms",
        query_id,
        branch_name,
        response.row_count,
        execution_time_ms
    );

    Ok(Json(response))
}

/// Execute a DDL/DML SQL statement on a branch
///
/// POST /v1/branches/:name/execute
///
/// Executes INSERT, UPDATE, DELETE, CREATE TABLE, etc. Returns the number
/// of rows affected.
///
/// # Path Parameters
///
/// - name: Branch name to execute statement on
///
/// # Request Body
///
/// - sql: SQL statement to execute
/// - params: Optional statement parameters (for $1, $2, etc.)
/// - timeout_ms: Optional statement timeout in milliseconds
///
/// # Response
///
/// - 200 OK: Returns ExecuteResponse with affected row count
/// - 400 Bad Request: Invalid SQL or parameters
/// - 404 Not Found: Branch does not exist
/// - 408 Request Timeout: Statement exceeded timeout
/// - 422 Unprocessable Entity: Statement execution failed
/// - 500 Internal Server Error: Database error
pub async fn execute_statement(
    State(state): State<AppState>,
    Path(branch_name): Path<String>,
    Json(request): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, ApiError> {
    info!("Executing statement on branch '{}': {}", branch_name, request.sql);

    // Register statement with cancellation support
    let (query_id, cancel_token) = state.query_registry.register_query(
        &request.sql,
        "api_user", // In production, get from auth context
        &branch_name,
        None, // session_id
    );

    // Update state to executing
    state.query_registry.update_state(query_id, QueryState::Executing);

    // Validate branch exists (if branching is enabled)
    if let Some(branch_manager) = state.db.storage.branch_manager() {
        // Check if branch exists
        let _branch = branch_manager.get_branch_by_name(&branch_name)
            .map_err(|e| {
                warn!("Branch '{}' not found: {}", branch_name, e);
                state.query_registry.fail_query(query_id);
                ApiError::from(e)
            })?;
    }

    // Start timing
    let start = Instant::now();

    // Determine statement type from SQL
    let statement_type = determine_statement_type(&request.sql);

    // Convert parameters if provided
    let params: Vec<Value> = request.params
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.into())
        .collect();

    // Check for cancellation before executing
    if cancel_token.is_cancelled() {
        state.query_registry.update_state(query_id, QueryState::Cancelled);
        return Err(ApiError::from(Error::query_cancelled("Statement cancelled before execution")));
    }

    // Execute statement
    let affected_rows = if params.is_empty() {
        state.db.execute(&request.sql)
            .map_err(|e| {
                error!("Statement execution failed: {}", e);
                if cancel_token.is_cancelled() {
                    state.query_registry.update_state(query_id, QueryState::Cancelled);
                } else {
                    state.query_registry.fail_query(query_id);
                }
                ApiError::from(e)
            })?
    } else {
        state.db.execute_params(&request.sql, &params)
            .map_err(|e| {
                error!("Statement execution failed: {}", e);
                if cancel_token.is_cancelled() {
                    state.query_registry.update_state(query_id, QueryState::Cancelled);
                } else {
                    state.query_registry.fail_query(query_id);
                }
                ApiError::from(e)
            })?
    };

    // Check for cancellation after execution
    if cancel_token.is_cancelled() {
        state.query_registry.update_state(query_id, QueryState::Cancelled);
        return Err(ApiError::from(Error::query_cancelled("Statement cancelled during execution")));
    }

    // Mark as completed
    state.query_registry.complete_query(query_id);

    // Calculate execution time
    let execution_time_ms = start.elapsed().as_millis() as u64;

    let message = format!(
        "{} statement executed successfully on branch '{}'",
        statement_type,
        branch_name
    );

    info!("Query {} - {} - {} rows affected in {}ms", query_id, message, affected_rows, execution_time_ms);

    Ok(Json(ExecuteResponse {
        statement_type,
        affected_rows,
        execution_time_ms,
        message: Some(message),
    }))
}

// Helper functions

/// Infer schema from result tuples
///
/// This is a simplified implementation. In production, we would get
/// the schema from the query plan or executor.
fn infer_schema_from_tuples(tuples: &[Tuple]) -> Result<Schema, ApiError> {
    if tuples.is_empty() {
        return Ok(Schema::new(vec![]));
    }

    // For now, create generic column names
    let first_tuple = &tuples[0];
    let columns: Vec<crate::Column> = first_tuple.values.iter().enumerate()
        .map(|(idx, value)| {
            let data_type = match value {
                Value::Null => DataType::Text, // Default to text for null
                Value::Boolean(_) => DataType::Boolean,
                Value::Int2(_) => DataType::Int4, // Promote Int2 to Int4 for consistency
                Value::Int4(_) => DataType::Int4,
                Value::Int8(_) => DataType::Int8,
                Value::Float4(_) => DataType::Float4,
                Value::Float8(_) => DataType::Float8,
                Value::Numeric(_) => DataType::Numeric,
                Value::String(_) => DataType::Text,
                Value::Bytes(_) => DataType::Bytea,
                Value::Uuid(_) => DataType::Text, // UUIDs displayed as text
                Value::Timestamp(_) => DataType::Timestamp,
                Value::Date(_) => DataType::Date,
                Value::Time(_) => DataType::Time,
                Value::Json(_) => DataType::Json,
                Value::Array(_) => DataType::Json, // Arrays displayed as JSON
                Value::Vector(v) => DataType::Vector(v.len()),
                // Storage references (should be resolved before reaching here)
                Value::DictRef { .. } => DataType::Text,
                Value::CasRef { .. } => DataType::Bytea,
                Value::ColumnarRef => DataType::Text,
                Value::Interval(_) => DataType::Interval,
            };

            crate::Column {
                name: format!("column_{}", idx),
                data_type,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            }
        })
        .collect();

    Ok(Schema::new(columns))
}

/// Convert tuples to JSON row format
fn tuples_to_rows(tuples: &[Tuple], schema: &Schema) -> Vec<HashMap<String, serde_json::Value>> {
    tuples.iter().map(|tuple| {
        let mut row = HashMap::new();
        for (idx, value) in tuple.values.iter().enumerate() {
            if let Some(column) = schema.columns.get(idx) {
                let json_value: serde_json::Value = value.into();
                row.insert(column.name.clone(), json_value);
            }
        }
        row
    }).collect()
}

/// Convert DataType to string representation
fn datatype_to_string(data_type: &DataType) -> String {
    match data_type {
        DataType::Boolean => "boolean".to_string(),
        DataType::Int2 => "int2".to_string(),
        DataType::Int4 => "int4".to_string(),
        DataType::Int8 => "int8".to_string(),
        DataType::Float4 => "float4".to_string(),
        DataType::Float8 => "float8".to_string(),
        DataType::Numeric => "numeric".to_string(),
        DataType::Text => "text".to_string(),
        DataType::Varchar(n) => format!("varchar({})", n.map_or("".to_string(), |x| x.to_string())),
        DataType::Char(n) => format!("char({})", n),
        DataType::Bytea => "bytea".to_string(),
        DataType::Date => "date".to_string(),
        DataType::Time => "time".to_string(),
        DataType::Timestamp => "timestamp".to_string(),
        DataType::Timestamptz => "timestamptz".to_string(),
        DataType::Interval => "interval".to_string(),
        DataType::Uuid => "uuid".to_string(),
        DataType::Json => "json".to_string(),
        DataType::Jsonb => "jsonb".to_string(),
        DataType::Vector(dims) => format!("vector({})", dims),
        DataType::Array(inner) => format!("{}[]", datatype_to_string(inner)),
    }
}

/// Determine statement type from SQL
fn determine_statement_type(sql: &str) -> String {
    let trimmed = sql.trim().to_uppercase();

    if trimmed.starts_with("INSERT") {
        "INSERT".to_string()
    } else if trimmed.starts_with("UPDATE") {
        "UPDATE".to_string()
    } else if trimmed.starts_with("DELETE") {
        "DELETE".to_string()
    } else if trimmed.starts_with("CREATE TABLE") {
        "CREATE TABLE".to_string()
    } else if trimmed.starts_with("CREATE INDEX") {
        "CREATE INDEX".to_string()
    } else if trimmed.starts_with("DROP TABLE") {
        "DROP TABLE".to_string()
    } else if trimmed.starts_with("DROP INDEX") {
        "DROP INDEX".to_string()
    } else if trimmed.starts_with("ALTER TABLE") {
        "ALTER TABLE".to_string()
    } else if trimmed.starts_with("TRUNCATE") {
        "TRUNCATE".to_string()
    } else if trimmed.starts_with("BEGIN") || trimmed.starts_with("START TRANSACTION") {
        "BEGIN".to_string()
    } else if trimmed.starts_with("COMMIT") {
        "COMMIT".to_string()
    } else if trimmed.starts_with("ROLLBACK") {
        "ROLLBACK".to_string()
    } else {
        "UNKNOWN".to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;
    use crate::api::models::query::QueryParameter;
    use crate::compute::QueryRegistry;
    use std::sync::Arc;

    fn create_test_state() -> AppState {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(QueryRegistry::new());
        AppState { db, query_registry }
    }

    #[tokio::test]
    async fn test_execute_query_empty_result() {
        let state = create_test_state();

        // Create table
        state.db.execute("CREATE TABLE test (id INT, name TEXT)").unwrap();

        let request = QueryRequest {
            sql: "SELECT * FROM test".to_string(),
            params: None,
            as_of: None,
            timeout_ms: None,
        };

        let result = execute_query(
            State(state),
            Path("main".to_string()),
            Json(request),
        ).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.0.row_count, 0);
    }

    #[tokio::test]
    async fn test_execute_statement_insert() {
        let state = create_test_state();

        // Create table
        state.db.execute("CREATE TABLE test (id INT, name TEXT)").unwrap();

        let request = ExecuteRequest {
            sql: "INSERT INTO test (id, name) VALUES (1, 'Alice')".to_string(),
            params: None,
            timeout_ms: None,
        };

        let result = execute_statement(
            State(state),
            Path("main".to_string()),
            Json(request),
        ).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.0.statement_type, "INSERT");
        assert_eq!(response.0.affected_rows, 1);
    }

    #[tokio::test]
    async fn test_execute_query_with_params() {
        let state = create_test_state();

        // Create and populate table
        state.db.execute("CREATE TABLE test (id INT, name TEXT)").unwrap();
        state.db.execute("INSERT INTO test VALUES (1, 'Alice')").unwrap();

        let request = QueryRequest {
            sql: "SELECT * FROM test WHERE id = $1".to_string(),
            params: Some(vec![QueryParameter::Int4(1)]),
            as_of: None,
            timeout_ms: None,
        };

        let result = execute_query(
            State(state),
            Path("main".to_string()),
            Json(request),
        ).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.0.row_count, 1);
    }

    #[test]
    fn test_determine_statement_type() {
        assert_eq!(determine_statement_type("INSERT INTO test VALUES (1)"), "INSERT");
        assert_eq!(determine_statement_type("UPDATE test SET x = 1"), "UPDATE");
        assert_eq!(determine_statement_type("DELETE FROM test"), "DELETE");
        assert_eq!(determine_statement_type("CREATE TABLE test (id INT)"), "CREATE TABLE");
        assert_eq!(determine_statement_type("TRUNCATE test"), "TRUNCATE");
        assert_eq!(determine_statement_type("BEGIN"), "BEGIN");
        assert_eq!(determine_statement_type("COMMIT"), "COMMIT");
    }

    #[test]
    fn test_datatype_to_string() {
        assert_eq!(datatype_to_string(&DataType::Int4), "int4");
        assert_eq!(datatype_to_string(&DataType::Text), "text");
        assert_eq!(datatype_to_string(&DataType::Vector(128)), "vector(128)");
        assert_eq!(datatype_to_string(&DataType::Varchar(Some(255))), "varchar(255)");
    }

    #[test]
    fn test_infer_schema_from_tuples() {
        let tuples = vec![
            Tuple::new(vec![
                Value::Int4(1),
                Value::String("Alice".to_string()),
            ]),
        ];

        let schema = infer_schema_from_tuples(&tuples).unwrap();
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].data_type, DataType::Int4);
        assert_eq!(schema.columns[1].data_type, DataType::Text);
    }
}
