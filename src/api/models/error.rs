//! API error types and HTTP status code mapping

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::Error;

/// API error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    /// HTTP status code
    #[serde(skip)]
    pub status: StatusCode,

    /// Error type/category
    pub error: String,

    /// Human-readable error message
    pub message: String,

    /// Optional details for debugging
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl ApiError {
    /// Create a new API error
    pub fn new(status: StatusCode, error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            error: error.into(),
            message: message.into(),
            details: None,
        }
    }

    /// Create an API error with details
    pub fn with_details(
        status: StatusCode,
        error: impl Into<String>,
        message: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self {
            status,
            error: error.into(),
            message: message.into(),
            details: Some(details.into()),
        }
    }

    /// Create a bad request error (400)
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BadRequest", message)
    }

    /// Create a not found error (404)
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NotFound", message)
    }

    /// Create a conflict error (409)
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "Conflict", message)
    }

    /// Create an internal server error (500)
    pub fn internal_server_error(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "InternalServerError", message)
    }

    /// Create an internal server error (500) - shorthand alias
    pub fn internal(message: impl Into<String>) -> Self {
        Self::internal_server_error(message)
    }

    /// Create an unprocessable entity error (422)
    pub fn unprocessable_entity(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "UnprocessableEntity", message)
    }

    /// Create an unauthorized error (401)
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Unauthorized", message)
    }
}

// Convert our domain Error to ApiError
impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        match err {
            Error::Storage(msg) => {
                if msg.contains("not found") || msg.contains("does not exist") {
                    ApiError::not_found(msg)
                } else if msg.contains("already exists") {
                    ApiError::conflict(msg)
                } else {
                    ApiError::internal_server_error(msg)
                }
            }
            Error::SqlParse(msg) => ApiError::bad_request(msg),
            Error::QueryExecution(msg) => ApiError::unprocessable_entity(msg),
            Error::QueryTimeout(msg) => {
                ApiError::new(StatusCode::REQUEST_TIMEOUT, "QueryTimeout", msg)
            }
            Error::QueryCancelled(msg) => {
                ApiError::new(StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST), "QueryCancelled", msg)
            }
            Error::Transaction(msg) => ApiError::unprocessable_entity(msg),
            Error::TypeConversion(msg) => ApiError::bad_request(msg),
            Error::Config(msg) => ApiError::bad_request(msg),
            Error::Protocol(msg) => ApiError::bad_request(msg),
            Error::BranchMerge(msg) => ApiError::unprocessable_entity(msg),
            Error::MergeConflict(msg) => ApiError::conflict(msg),
            Error::ConstraintViolation(msg) => ApiError::conflict(msg),
            Error::Encryption(_) | Error::VectorIndex(_) | Error::MultiTenant(_)
            | Error::Audit(_) | Error::Compression(_) | Error::LockPoisoned(_)
            | Error::Generic(_) => {
                ApiError::internal_server_error(format!("{}", err))
            }
            Error::Io(e) => ApiError::internal_server_error(format!("I/O error: {}", e)),
        }
    }
}

// Implement IntoResponse so ApiError can be returned from handlers
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let body = Json(self);
        (status, body).into_response()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_api_error_creation() {
        let err = ApiError::bad_request("Invalid input");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error, "BadRequest");
        assert_eq!(err.message, "Invalid input");
        assert!(err.details.is_none());
    }

    #[test]
    fn test_api_error_with_details() {
        let err = ApiError::with_details(
            StatusCode::BAD_REQUEST,
            "ValidationError",
            "Invalid branch name",
            "Branch name must be alphanumeric",
        );
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error, "ValidationError");
        assert_eq!(err.message, "Invalid branch name");
        assert_eq!(err.details, Some("Branch name must be alphanumeric".to_string()));
    }

    #[test]
    fn test_error_conversion_storage_not_found() {
        let domain_err = Error::storage("Branch 'dev' not found");
        let api_err: ApiError = domain_err.into();
        assert_eq!(api_err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_error_conversion_storage_exists() {
        let domain_err = Error::storage("Branch already exists");
        let api_err: ApiError = domain_err.into();
        assert_eq!(api_err.status, StatusCode::CONFLICT);
    }

    #[test]
    fn test_error_conversion_sql_parse() {
        let domain_err = Error::sql_parse("Invalid SQL syntax");
        let api_err: ApiError = domain_err.into();
        assert_eq!(api_err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_error_conversion_merge_conflict() {
        let domain_err = Error::merge_conflict("Conflicts detected");
        let api_err: ApiError = domain_err.into();
        assert_eq!(api_err.status, StatusCode::CONFLICT);
    }

    #[test]
    fn test_error_serialization() {
        let err = ApiError::bad_request("Test error");
        let json = serde_json::to_string(&err).unwrap();
        let deserialized: ApiError = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.error, "BadRequest");
        assert_eq!(deserialized.message, "Test error");
    }
}
