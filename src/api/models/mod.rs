//! Request and Response models for REST API

use serde::{Serialize, Deserialize};

/// Generic API response wrapper for consistent response format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    /// Success status flag
    pub success: bool,
    /// Response data
    pub data: Option<T>,
    /// Error message if failed
    pub error: Option<String>,
    /// Optional metadata
    pub meta: Option<serde_json::Value>,
}

impl<T: Serialize> ApiResponse<T> {
    /// Create a successful response
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta: None,
        }
    }

    /// Create a successful response (alias for ok)
    pub fn success(data: T) -> Self {
        Self::ok(data)
    }

    /// Create a successful response with metadata
    pub fn ok_with_meta(data: T, meta: serde_json::Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta: Some(meta),
        }
    }

    /// Create an error response
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message.into()),
            meta: None,
        }
    }
}

// Response types for specific operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchResult {
    pub id: String,
    pub score: f32,
    pub values: Option<Vec<f32>>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSearchResults {
    pub results: Vec<(String, f32)>,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagSource {
    pub id: String,
    pub content: String,
    pub score: f32,
    pub metadata: Option<serde_json::Value>,
    pub context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResult {
    pub id: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: ChatCompletionMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionMessage {
    pub role: String,
    pub content: String,
    pub name: Option<String>,
    pub function_call: Option<serde_json::Value>,
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResult {
    pub embeddings: Vec<Vec<f32>>,
    pub model: String,
    pub prompt_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: u32,
    pub total_tokens: u32,
}

pub mod branch;
pub mod error;
pub mod query;
pub mod auth;
pub mod data;
pub mod cancellation;

// Re-exports
pub use branch::{
    CreateBranchRequest,
    BranchResponse,
    BranchListResponse,
    MergeBranchRequest,
    MergeBranchResponse,
    MergeStrategyDto,
    BranchStateDto,
    BranchStatsDto,
    MergeConflictDto,
};
pub use error::ApiError;
pub use query::{
    QueryRequest,
    QueryResponse,
    ExecuteRequest,
    ExecuteResponse,
    QueryParameter,
    AsOfSpec,
};
pub use auth::{
    ApiKeyAuth,
    JwtAuth,
    UserContextResponse,
    RateLimitInfoResponse,
    LoginRequest,
    LoginResponse,
    RefreshTokenRequest,
    RefreshTokenResponse,
    CreateApiKeyRequest,
    CreateApiKeyResponse,
    ApiKeyListItem,
};
pub use data::{
    TableListResponse,
    TableInfoResponse,
    DataQueryParams,
    DataQueryResponse,
    InsertDataRequest,
    InsertDataResponse,
    UpdateDataRequest,
    UpdateDataResponse,
    DeleteDataRequest,
    DeleteDataResponse,
    BatchInferResponse,
    OptimizationResponse,
    SchemaComparisonResponse,
    NaturalLanguageSchemaResponse,
};
pub use cancellation::{
    CancelQueryRequest,
    CancelQueryResponse,
    RunningQueryInfo,
    RunningQueriesResponse,
    QueryStatusResponse,
    CancelSessionQueriesRequest,
    BulkCancelResponse,
};
