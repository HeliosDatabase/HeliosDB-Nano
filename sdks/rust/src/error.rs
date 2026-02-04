//! Error types for HeliosDB client

use thiserror::Error;

/// HeliosDB client error
#[derive(Error, Debug)]
pub enum Error {
    /// HTTP request error
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// URL parsing error
    #[error("Invalid URL: {0}")]
    Url(#[from] url::ParseError),

    /// JSON serialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// API error response
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },

    /// Query error
    #[error("Query error: {0}")]
    Query(String),

    /// Authentication error
    #[error("Authentication failed")]
    Authentication,

    /// Not found error
    #[error("Not found: {0}")]
    NotFound(String),

    /// Rate limit exceeded
    #[error("Rate limit exceeded")]
    RateLimit,

    /// Timeout error
    #[error("Request timeout")]
    Timeout,

    /// Connection error
    #[error("Connection error: {0}")]
    Connection(String),
}

/// Result type alias
pub type Result<T> = std::result::Result<T, Error>;
