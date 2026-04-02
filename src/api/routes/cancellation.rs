//! Query cancellation routes
//!
//! Provides REST endpoints for managing and cancelling running queries.

use axum::{
    Router,
    routing::{get, post},
};

use crate::api::{
    handlers::cancellation_handler,
    server::AppState,
};

/// Create query cancellation routes
///
/// # Endpoints
///
/// - `GET /v1/queries` - List running queries
/// - `GET /v1/queries/stats` - Get query statistics
/// - `GET /v1/queries/:query_id` - Get specific query status
/// - `POST /v1/queries/:query_id/cancel` - Cancel a specific query
/// - `POST /v1/queries/cancel-session` - Cancel all queries for a session
/// - `POST /v1/queries/cancel-timed-out` - Cancel timed out queries
///
/// # List Running Queries (GET /v1/queries)
///
/// Lists all currently running queries with filtering options.
///
/// **Query Parameters:**
/// - `user` - Filter by user name
/// - `database` - Filter by database name
/// - `state` - Filter by state (planning, executing, cancelling)
/// - `include_completed` - Include completed queries (default: false)
/// - `limit` - Maximum results (default: 100)
///
/// **Response:**
/// ```json
/// {
///   "queries": [
///     {
///       "query_id": 123,
///       "sql": "SELECT * FROM users WHERE...",
///       "user_name": "alice",
///       "database": "main",
///       "state": "executing",
///       "started_at": "2024-01-01T12:00:00Z",
///       "elapsed_ms": 5000,
///       "rows_processed": 10000,
///       "cancellable": true
///     }
///   ],
///   "total": 1
/// }
/// ```
///
/// # Cancel Query (POST /v1/queries/:query_id/cancel)
///
/// Requests cancellation of a running query.
///
/// **Request Body:**
/// ```json
/// {
///   "reason": "User requested cancellation"
/// }
/// ```
///
/// **Response:**
/// ```json
/// {
///   "query_id": 123,
///   "cancelled": true,
///   "message": "Cancellation requested. Query will terminate shortly."
/// }
/// ```
///
/// # Cancel Session Queries (POST /v1/queries/cancel-session)
///
/// Cancels all queries associated with a specific session.
///
/// **Request Body:**
/// ```json
/// {
///   "session_id": 42,
///   "reason": "Session terminated"
/// }
/// ```
///
/// **Response:**
/// ```json
/// {
///   "cancelled_count": 3,
///   "message": "Cancelled 3 queries for session 42"
/// }
/// ```
pub fn routes() -> Router<AppState> {
    Router::new()
        // List and stats
        .route("/", get(cancellation_handler::list_running_queries))
        .route("/stats", get(cancellation_handler::get_query_stats))
        // Single query operations
        .route("/:query_id", get(cancellation_handler::get_query_status))
        .route("/:query_id/cancel", post(cancellation_handler::cancel_query))
        // Bulk operations
        .route("/cancel-session", post(cancellation_handler::cancel_session_queries))
        .route("/cancel-timed-out", post(cancellation_handler::cancel_timed_out_queries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;
    use crate::compute::QueryRegistry;
    use std::sync::Arc;

    #[test]
    fn test_routes_creation() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(QueryRegistry::new());
        let state = AppState { db, query_registry, auth_bridge: None, oauth_registry: None, change_notifier: None };
        let router: axum::Router<()> = routes().with_state(state);
        drop(router);
    }
}
