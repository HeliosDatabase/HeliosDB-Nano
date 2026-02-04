//! Query cancellation models
//!
//! Request and response models for query cancellation operations.

use serde::{Deserialize, Serialize};

/// Request to cancel a query
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CancelQueryRequest {
    /// Optional reason for cancellation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Response after cancelling a query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelQueryResponse {
    /// Query ID that was cancelled
    pub query_id: u64,
    /// Whether the cancellation was successful
    pub cancelled: bool,
    /// Message describing the result
    pub message: String,
}

/// Information about a running query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningQueryInfo {
    /// Unique query ID
    pub query_id: u64,
    /// SQL text (possibly truncated)
    pub sql: String,
    /// Session ID (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// User name
    pub user_name: String,
    /// Database name
    pub database: String,
    /// Query state
    pub state: String,
    /// When the query started (ISO 8601)
    pub started_at: String,
    /// Elapsed time in milliseconds
    pub elapsed_ms: u64,
    /// Rows processed so far
    pub rows_processed: u64,
    /// Whether the query can be cancelled
    pub cancellable: bool,
}

/// Response listing running queries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningQueriesResponse {
    /// List of running queries
    pub queries: Vec<RunningQueryInfo>,
    /// Total count of running queries
    pub total: usize,
}

/// Response for getting a single query's status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryStatusResponse {
    /// Query information
    pub query: RunningQueryInfo,
    /// Whether the query is still running
    pub is_running: bool,
}

/// Request to cancel all queries for a session
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CancelSessionQueriesRequest {
    /// Session ID to cancel queries for
    pub session_id: u64,
    /// Optional reason for cancellation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Response for bulk cancellation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkCancelResponse {
    /// Number of queries cancelled
    pub cancelled_count: usize,
    /// Message describing the result
    pub message: String,
}

impl From<&crate::compute::RunningQuery> for RunningQueryInfo {
    fn from(query: &crate::compute::RunningQuery) -> Self {
        Self {
            query_id: query.query_id,
            sql: query.sql.clone(),
            session_id: query.session_id,
            user_name: query.user_name.clone(),
            database: query.database.clone(),
            state: query.state.to_string(),
            started_at: query.started_at.to_rfc3339(),
            elapsed_ms: query.elapsed.as_millis() as u64,
            rows_processed: query.rows_processed,
            cancellable: query.cancellable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancel_query_request_serialization() {
        let request = CancelQueryRequest {
            reason: Some("User requested".to_string()),
        };

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: CancelQueryRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.reason, Some("User requested".to_string()));
    }

    #[test]
    fn test_running_queries_response_serialization() {
        let response = RunningQueriesResponse {
            queries: vec![RunningQueryInfo {
                query_id: 1,
                sql: "SELECT * FROM users".to_string(),
                session_id: Some(42),
                user_name: "alice".to_string(),
                database: "test".to_string(),
                state: "executing".to_string(),
                started_at: "2024-01-01T00:00:00Z".to_string(),
                elapsed_ms: 1000,
                rows_processed: 100,
                cancellable: true,
            }],
            total: 1,
        };

        let json = serde_json::to_string(&response).unwrap();
        let deserialized: RunningQueriesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.total, 1);
        assert_eq!(deserialized.queries[0].query_id, 1);
    }
}
