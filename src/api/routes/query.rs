//! Query execution routes
//!
//! Provides REST endpoints for executing SQL queries and statements.

use axum::{
    Router,
    routing::post,
};

use crate::api::{
    handlers::query_handler,
    server::AppState,
};

/// Create query execution routes
///
/// # Endpoints
///
/// - `POST /v1/branches/:name/query` - Execute read-only SQL query
/// - `POST /v1/branches/:name/execute` - Execute DDL/DML statement
///
/// # Query Endpoint (POST /v1/branches/:name/query)
///
/// Executes a SELECT query and returns results. Supports:
/// - Parameterized queries ($1, $2, etc.)
/// - Time-travel queries (AS OF)
/// - Query timeout configuration
///
/// **Request Body:**
/// ```json
/// {
///   "sql": "SELECT * FROM users WHERE id = $1",
///   "params": [{"type": "int4", "value": 1}],
///   "as_of": {"type": "timestamp", "value": 1234567890},
///   "timeout_ms": 5000
/// }
/// ```
///
/// **Response:**
/// ```json
/// {
///   "columns": ["id", "name", "email"],
///   "column_types": ["int4", "text", "text"],
///   "rows": [
///     {"id": 1, "name": "Alice", "email": "alice@example.com"}
///   ],
///   "row_count": 1,
///   "execution_time_ms": 42
/// }
/// ```
///
/// # Execute Endpoint (POST /v1/branches/:name/execute)
///
/// Executes DDL/DML statements (INSERT, UPDATE, DELETE, CREATE TABLE, etc.).
///
/// **Request Body:**
/// ```json
/// {
///   "sql": "INSERT INTO users (id, name) VALUES ($1, $2)",
///   "params": [
///     {"type": "int4", "value": 1},
///     {"type": "string", "value": "Alice"}
///   ],
///   "timeout_ms": 5000
/// }
/// ```
///
/// **Response:**
/// ```json
/// {
///   "statement_type": "INSERT",
///   "affected_rows": 1,
///   "execution_time_ms": 23,
///   "message": "INSERT statement executed successfully on branch 'main'"
/// }
/// ```
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/:name/query", post(query_handler::execute_query))
        .route("/:name/execute", post(query_handler::execute_statement))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;
    use std::sync::Arc;

    #[test]
    fn test_routes_creation() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(crate::compute::QueryRegistry::new());
        let state = AppState { db, query_registry };
        let router: axum::Router<()> = routes().with_state(state);
        // Router created successfully
        drop(router);
    }
}
