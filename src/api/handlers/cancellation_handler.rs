//! Query cancellation handlers
//!
//! Implements HTTP handlers for query cancellation operations.

use axum::{
    extract::{Path, State, Query},
    Json,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::api::{
    models::{
        ApiError,
        CancelQueryRequest,
        CancelQueryResponse,
        RunningQueryInfo,
        RunningQueriesResponse,
        QueryStatusResponse,
        CancelSessionQueriesRequest,
        BulkCancelResponse,
    },
    server::AppState,
};

/// Query parameters for listing running queries
#[derive(Debug, Deserialize)]
pub struct ListQueriesParams {
    /// Filter by user name
    #[serde(default)]
    pub user: Option<String>,
    /// Filter by database
    #[serde(default)]
    pub database: Option<String>,
    /// Filter by state (planning, executing, cancelling)
    #[serde(default)]
    pub state: Option<String>,
    /// Include completed queries
    #[serde(default)]
    pub include_completed: bool,
    /// Maximum number of results
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

/// List all running queries
///
/// GET /v1/queries
///
/// Returns a list of all currently running queries with their status.
///
/// # Query Parameters
///
/// - user: Filter by user name
/// - database: Filter by database
/// - state: Filter by query state (planning, executing, cancelling)
/// - include_completed: Include recently completed queries (default: false)
/// - limit: Maximum results to return (default: 100)
///
/// # Response
///
/// - 200 OK: Returns RunningQueriesResponse with list of queries
pub async fn list_running_queries(
    State(state): State<AppState>,
    Query(params): Query<ListQueriesParams>,
) -> Result<Json<RunningQueriesResponse>, ApiError> {
    let queries = if params.include_completed {
        state.query_registry.list_all_queries()
    } else {
        state.query_registry.list_running_queries()
    };

    // Apply filters
    let filtered: Vec<RunningQueryInfo> = queries
        .iter()
        .filter(|q| {
            if let Some(ref user) = params.user {
                if q.user_name != *user {
                    return false;
                }
            }
            if let Some(ref db) = params.database {
                if q.database != *db {
                    return false;
                }
            }
            if let Some(ref state_filter) = params.state {
                if q.state.to_string() != *state_filter {
                    return false;
                }
            }
            true
        })
        .take(params.limit)
        .map(RunningQueryInfo::from)
        .collect();

    let total = filtered.len();

    info!("Listed {} running queries", total);

    Ok(Json(RunningQueriesResponse {
        queries: filtered,
        total,
    }))
}

/// Get status of a specific query
///
/// GET /v1/queries/:query_id
///
/// Returns detailed information about a specific query.
///
/// # Path Parameters
///
/// - query_id: The query ID to look up
///
/// # Response
///
/// - 200 OK: Returns QueryStatusResponse with query details
/// - 404 Not Found: Query not found
pub async fn get_query_status(
    State(state): State<AppState>,
    Path(query_id): Path<u64>,
) -> Result<Json<QueryStatusResponse>, ApiError> {
    let query = state.query_registry.get_query(query_id)
        .ok_or_else(|| ApiError::not_found(format!("Query {} not found", query_id)))?;

    let is_running = matches!(
        query.state,
        crate::compute::QueryState::Planning | crate::compute::QueryState::Executing
    );

    Ok(Json(QueryStatusResponse {
        query: RunningQueryInfo::from(&query),
        is_running,
    }))
}

/// Cancel a specific query
///
/// POST /v1/queries/:query_id/cancel
///
/// Requests cancellation of a running query. The query will be cancelled
/// cooperatively - it may take a moment for the query to actually terminate.
///
/// # Path Parameters
///
/// - query_id: The query ID to cancel
///
/// # Request Body
///
/// - reason: Optional reason for cancellation
///
/// # Response
///
/// - 200 OK: Returns CancelQueryResponse indicating cancellation was requested
/// - 404 Not Found: Query not found
/// - 400 Bad Request: Query cannot be cancelled
pub async fn cancel_query(
    State(state): State<AppState>,
    Path(query_id): Path<u64>,
    Json(request): Json<CancelQueryRequest>,
) -> Result<Json<CancelQueryResponse>, ApiError> {
    info!("Cancellation requested for query {}", query_id);

    let cancelled = if let Some(reason) = request.reason {
        state.query_registry.cancel_query_with_reason(query_id, &reason)
    } else {
        state.query_registry.cancel_query(query_id)
    }.map_err(|e| {
        warn!("Failed to cancel query {}: {}", query_id, e);
        ApiError::bad_request(e.to_string())
    })?;

    if cancelled {
        info!("Query {} cancellation requested successfully", query_id);
        Ok(Json(CancelQueryResponse {
            query_id,
            cancelled: true,
            message: "Cancellation requested. Query will terminate shortly.".to_string(),
        }))
    } else {
        warn!("Query {} not found for cancellation", query_id);
        Err(ApiError::not_found(format!("Query {} not found or already completed", query_id)))
    }
}

/// Cancel all queries for a specific session
///
/// POST /v1/queries/cancel-session
///
/// Cancels all running queries associated with a specific session ID.
///
/// # Request Body
///
/// - session_id: The session ID whose queries should be cancelled
/// - reason: Optional reason for cancellation
///
/// # Response
///
/// - 200 OK: Returns BulkCancelResponse with count of cancelled queries
pub async fn cancel_session_queries(
    State(state): State<AppState>,
    Json(request): Json<CancelSessionQueriesRequest>,
) -> Result<Json<BulkCancelResponse>, ApiError> {
    info!("Cancelling all queries for session {}", request.session_id);

    let cancelled_count = state.query_registry.cancel_session_queries(request.session_id);

    info!("Cancelled {} queries for session {}", cancelled_count, request.session_id);

    Ok(Json(BulkCancelResponse {
        cancelled_count,
        message: format!(
            "Cancelled {} queries for session {}",
            cancelled_count,
            request.session_id
        ),
    }))
}

/// Cancel all queries exceeding a timeout
///
/// POST /v1/queries/cancel-timed-out
///
/// Cancels all queries that have been running longer than the specified timeout.
/// This is typically called by an administrator or automated process.
///
/// # Query Parameters
///
/// - timeout_secs: Timeout in seconds (default: 300)
///
/// # Response
///
/// - 200 OK: Returns BulkCancelResponse with count of cancelled queries
#[derive(Debug, Deserialize)]
pub struct CancelTimeoutParams {
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    300 // 5 minutes
}

pub async fn cancel_timed_out_queries(
    State(state): State<AppState>,
    Query(params): Query<CancelTimeoutParams>,
) -> Result<Json<BulkCancelResponse>, ApiError> {
    let timeout = std::time::Duration::from_secs(params.timeout_secs);

    info!("Cancelling queries exceeding {}s timeout", params.timeout_secs);

    let cancelled_count = state.query_registry.cancel_timed_out_queries(timeout);

    info!("Cancelled {} timed out queries", cancelled_count);

    Ok(Json(BulkCancelResponse {
        cancelled_count,
        message: format!(
            "Cancelled {} queries exceeding {}s timeout",
            cancelled_count,
            params.timeout_secs
        ),
    }))
}

/// Get running query count statistics
///
/// GET /v1/queries/stats
///
/// Returns basic statistics about running queries.
///
/// # Response
///
/// - 200 OK: Returns statistics about running queries
pub async fn get_query_stats(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let running_count = state.query_registry.running_count();
    let all_queries = state.query_registry.list_all_queries();

    let planning = all_queries.iter()
        .filter(|q| matches!(q.state, crate::compute::QueryState::Planning))
        .count();
    let executing = all_queries.iter()
        .filter(|q| matches!(q.state, crate::compute::QueryState::Executing))
        .count();
    let cancelling = all_queries.iter()
        .filter(|q| matches!(q.state, crate::compute::QueryState::Cancelling))
        .count();

    Json(serde_json::json!({
        "running_count": running_count,
        "planning": planning,
        "executing": executing,
        "cancelling": cancelling,
        "total_tracked": all_queries.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_queries_params_defaults() {
        let params: ListQueriesParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.limit, 100);
        assert!(!params.include_completed);
        assert!(params.user.is_none());
    }

    #[test]
    fn test_cancel_timeout_params_defaults() {
        let params: CancelTimeoutParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.timeout_secs, 300);
    }
}
