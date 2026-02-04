//! Document API handlers
//!
//! Provides REST API endpoints for document management:
//! - Document CRUD operations
//! - Document chunking and embedding
//! - Full-text search with highlighting
//! - Semantic document search

#![allow(unused_variables)]

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::api::models::{ApiError, ApiResponse};
use crate::api::server::AppState;

/// Document metadata
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Document {
    /// Document ID
    pub id: String,
    /// Document content
    pub content: String,
    /// Document metadata
    pub metadata: HashMap<String, serde_json::Value>,
    /// Creation timestamp
    pub created_at: String,
    /// Update timestamp
    pub updated_at: String,
    /// Document chunks (if chunked)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<DocumentChunk>>,
}

/// Document chunk
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DocumentChunk {
    /// Chunk ID
    pub id: String,
    /// Chunk content
    pub content: String,
    /// Chunk index within document
    pub index: usize,
    /// Character offset in original document
    pub start_offset: usize,
    /// Character end offset
    pub end_offset: usize,
    /// Chunk metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Create document request
#[derive(Debug, Deserialize)]
pub struct CreateDocumentRequest {
    /// Optional document ID (auto-generated if not provided)
    pub id: Option<String>,
    /// Document content
    pub content: String,
    /// Document metadata
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    /// Chunking configuration
    pub chunking: Option<ChunkingConfig>,
    /// Auto-embed chunks
    #[serde(default = "default_true")]
    pub embed: bool,
    /// Vector store to embed into
    pub vector_store: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Chunking configuration
#[derive(Debug, Deserialize, Clone)]
pub struct ChunkingConfig {
    /// Chunking strategy (fixed, sentence, paragraph, semantic)
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Chunk size (in chars or tokens depending on strategy)
    #[serde(default = "default_chunk_size")]
    pub chunk_size: usize,
    /// Overlap between chunks
    #[serde(default = "default_overlap")]
    pub overlap: usize,
    /// Split on headers for markdown
    #[serde(default)]
    pub split_on_headers: bool,
    /// Minimum chunk size
    pub min_chunk_size: Option<usize>,
}

fn default_strategy() -> String {
    "sentence".to_string()
}

fn default_chunk_size() -> usize {
    512
}

fn default_overlap() -> usize {
    50
}

/// Batch create documents request
#[derive(Debug, Deserialize)]
pub struct BatchCreateRequest {
    /// Documents to create
    pub documents: Vec<CreateDocumentRequest>,
    /// Chunking config for all
    pub chunking: Option<ChunkingConfig>,
    /// Vector store to embed into
    pub vector_store: Option<String>,
}

/// Update document request
#[derive(Debug, Deserialize)]
pub struct UpdateDocumentRequest {
    /// New content (optional)
    pub content: Option<String>,
    /// Metadata to merge
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// Re-chunk document
    #[serde(default)]
    pub rechunk: bool,
    /// Re-embed document
    #[serde(default)]
    pub reembed: bool,
}

/// Search documents request
#[derive(Debug, Deserialize)]
pub struct SearchDocumentsRequest {
    /// Search query
    pub query: String,
    /// Search type (semantic, fulltext, hybrid)
    #[serde(default = "default_search_type")]
    pub search_type: String,
    /// Maximum results
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Metadata filter
    pub filter: Option<HashMap<String, serde_json::Value>>,
    /// Include content in response
    #[serde(default = "default_true")]
    pub include_content: bool,
    /// Include chunks in response
    #[serde(default)]
    pub include_chunks: bool,
    /// Highlight matches
    #[serde(default)]
    pub highlight: bool,
    /// Hybrid alpha (0=fulltext, 1=semantic)
    pub alpha: Option<f32>,
}

fn default_search_type() -> String {
    "hybrid".to_string()
}

fn default_limit() -> usize {
    10
}

/// Search result
#[derive(Debug, Serialize)]
pub struct DocumentSearchResult {
    /// Document ID
    pub id: String,
    /// Relevance score
    pub score: f32,
    /// Document content (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Highlighted content (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlights: Option<Vec<String>>,
    /// Document metadata
    pub metadata: HashMap<String, serde_json::Value>,
    /// Matching chunks (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<ChunkSearchResult>>,
}

/// Chunk search result
#[derive(Debug, Serialize)]
pub struct ChunkSearchResult {
    /// Chunk ID
    pub chunk_id: String,
    /// Chunk score
    pub score: f32,
    /// Chunk content
    pub content: String,
    /// Highlighted content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight: Option<String>,
    /// Chunk index
    pub index: usize,
}

/// Search response
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// Search results
    pub results: Vec<DocumentSearchResult>,
    /// Total matches
    pub total: usize,
    /// Query time in ms
    pub query_time_ms: u64,
}

/// List documents query
#[derive(Debug, Deserialize)]
pub struct ListDocumentsQuery {
    /// Maximum results
    pub limit: Option<usize>,
    /// Offset for pagination
    pub offset: Option<usize>,
    /// Sort field
    pub sort: Option<String>,
    /// Sort order (asc/desc)
    pub order: Option<String>,
    /// Filter by metadata
    pub filter: Option<String>,
}

/// Chunk document request
#[derive(Debug, Deserialize)]
pub struct ChunkDocumentRequest {
    /// Chunking configuration
    pub config: ChunkingConfig,
    /// Re-embed chunks
    #[serde(default = "default_true")]
    pub embed: bool,
    /// Vector store
    pub vector_store: Option<String>,
}

/// Similar documents request
#[derive(Debug, Deserialize)]
pub struct SimilarDocumentsRequest {
    /// Number of similar docs
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Include content
    #[serde(default)]
    pub include_content: bool,
}

// ============================================================================
// Handler implementations
// ============================================================================

/// List all documents
pub async fn list_documents(
    State(state): State<AppState>,
    Query(query): Query<ListDocumentsQuery>,
) -> Result<Json<ApiResponse<Vec<Document>>>, ApiError> {
    let docs = state.db.list_documents(
        "default",
    ).map_err(|e| ApiError::internal(format!("Failed to list documents: {}", e)))?;

    let documents: Vec<Document> = docs
        .into_iter()
        .map(|d| {
            // Convert Option<serde_json::Value> to HashMap
            let metadata = match d.metadata {
                Some(serde_json::Value::Object(map)) => map.into_iter().collect(),
                _ => HashMap::new(),
            };

            Document {
                id: d.id,
                content: d.content,
                metadata,
                created_at: d.created_at,
                updated_at: d.updated_at,
                chunks: None,
            }
        })
        .collect();

    Ok(Json(ApiResponse::success(documents)))
}

/// Create a document
pub async fn create_document(
    State(state): State<AppState>,
    Json(req): Json<CreateDocumentRequest>,
) -> Result<(StatusCode, Json<ApiResponse<Document>>), ApiError> {
    let id = req.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Convert HashMap to Option<serde_json::Value> for DB
    let metadata_value = if req.metadata.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(
            req.metadata.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        ))
    };

    let _doc_id = state.db.create_document(
        "default",
        &id,
        &req.content,
        metadata_value,
    ).map_err(|e| ApiError::internal(format!("Failed to create document: {}", e)))?;

    let document = Document {
        id: id.clone(),
        content: req.content.clone(),
        metadata: req.metadata,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        chunks: None,
    };

    Ok((StatusCode::CREATED, Json(ApiResponse::success(document))))
}

/// Batch create documents
pub async fn batch_create_documents(
    State(state): State<AppState>,
    Json(req): Json<BatchCreateRequest>,
) -> Result<(StatusCode, Json<ApiResponse<serde_json::Value>>), ApiError> {
    let docs_data: Vec<crate::types::DocumentData> = req.documents.iter().map(|d| {
        // Convert HashMap to serde_json::Value
        let metadata = if d.metadata.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(
                d.metadata.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            ))
        };

        crate::types::DocumentData {
            id: d.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            content: d.content.clone(),
            metadata,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            chunks: vec![],
        }
    }).collect();

    let ids = state.db.batch_create_documents(
        "default",
        docs_data,
    ).map_err(|e| ApiError::internal(format!("Failed to batch create: {}", e)))?;
    let count = ids.len();

    Ok((StatusCode::CREATED, Json(ApiResponse::success(serde_json::json!({
        "created_count": count,
    })))))
}

/// Get document by ID
pub async fn get_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<ApiResponse<Document>>, ApiError> {
    let include_chunks = params.get("include_chunks")
        .map(|s| s == "true")
        .unwrap_or(false);

    let doc = state.db.get_document("default", &doc_id)
        .map_err(|e| ApiError::not_found(format!("Document not found: {}", e)))?;

    // Convert Option<serde_json::Value> to HashMap
    let metadata = match doc.metadata {
        Some(serde_json::Value::Object(map)) => map.into_iter().collect(),
        _ => HashMap::new(),
    };

    let document = Document {
        id: doc.id,
        content: doc.content,
        metadata,
        created_at: doc.created_at,
        updated_at: doc.updated_at,
        chunks: if include_chunks {
            // Note: doc.chunks is Vec<String>, not Option<Vec<ChunkStruct>>
            // Would need to fetch actual chunk data separately
            None
        } else {
            None
        },
    };

    Ok(Json(ApiResponse::success(document)))
}

/// Update document
pub async fn update_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(req): Json<UpdateDocumentRequest>,
) -> Result<Json<ApiResponse<Document>>, ApiError> {
    // Convert HashMap to Option<serde_json::Value> for DB
    let metadata_value = req.metadata.clone().map(|map| {
        serde_json::Value::Object(
            map.into_iter().collect()
        )
    });

    state.db.update_document(
        "default",
        &doc_id,
        req.content.as_deref().unwrap_or(""),
        metadata_value,
    ).map_err(|e| ApiError::internal(format!("Failed to update document: {}", e)))?;

    let document = Document {
        id: doc_id.clone(),
        content: req.content.clone().unwrap_or_default(),
        metadata: req.metadata.unwrap_or_default(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        chunks: None,
    };

    Ok(Json(ApiResponse::success(document)))
}

/// Delete document
pub async fn delete_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.db.delete_document("default", &doc_id)
        .map_err(|e| ApiError::internal(format!("Failed to delete document: {}", e)))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Search documents
pub async fn search_documents(
    State(state): State<AppState>,
    Json(req): Json<SearchDocumentsRequest>,
) -> Result<Json<ApiResponse<SearchResponse>>, ApiError> {
    let start = std::time::Instant::now();

    let raw_results = state.db.search_documents(
        "default",
        &req.query,
    ).map_err(|e| ApiError::internal(format!("Search failed: {}", e)))?;

    let total = raw_results.len();

    let search_results: Vec<DocumentSearchResult> = raw_results
        .into_iter()
        .map(|doc| {
            // Convert metadata
            let metadata = match doc.metadata {
                Some(serde_json::Value::Object(map)) => map.into_iter().collect(),
                _ => HashMap::new(),
            };

            DocumentSearchResult {
                id: doc.id,
                score: 1.0, // Default score since search_documents doesn't return scores
                content: if req.include_content { Some(doc.content) } else { None },
                highlights: None, // Highlighting not implemented yet
                metadata,
                chunks: None, // Chunks would need separate fetch
            }
        })
        .collect();

    Ok(Json(ApiResponse::success(SearchResponse {
        results: search_results,
        total,
        query_time_ms: start.elapsed().as_millis() as u64,
    })))
}

/// Get document chunks
pub async fn get_chunks(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<DocumentChunk>>>, ApiError> {
    let chunks = state.db.get_document_chunks("default", &doc_id)
        .map_err(|e| ApiError::internal(format!("Failed to get chunks: {}", e)))?;

    // Convert (String, f32) tuples to DocumentChunk
    let result: Vec<DocumentChunk> = chunks
        .into_iter()
        .enumerate()
        .map(|(index, (id, _score))| DocumentChunk {
            id,
            content: String::new(), // Would need separate fetch for content
            index,
            start_offset: 0,
            end_offset: 0,
            metadata: None,
        })
        .collect();

    Ok(Json(ApiResponse::success(result)))
}

/// Re-chunk document
pub async fn chunk_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(req): Json<ChunkDocumentRequest>,
) -> Result<Json<ApiResponse<Vec<DocumentChunk>>>, ApiError> {
    let chunk_ids = state.db.rechunk_document(
        "default",
        &doc_id,
        512,
    ).map_err(|e| ApiError::internal(format!("Failed to chunk document: {}", e)))?;

    // Convert chunk IDs to DocumentChunk structs
    // Note: This is a placeholder until full chunk metadata is available
    let result: Vec<DocumentChunk> = chunk_ids
        .into_iter()
        .enumerate()
        .map(|(index, id)| DocumentChunk {
            id: id.clone(),
            content: String::new(), // Placeholder - would need to fetch actual content
            index,
            start_offset: 0,
            end_offset: 0,
            metadata: None,
        })
        .collect();

    Ok(Json(ApiResponse::success(result)))
}

/// Find similar documents
pub async fn similar_documents(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(req): Json<SimilarDocumentsRequest>,
) -> Result<Json<ApiResponse<Vec<DocumentSearchResult>>>, ApiError> {
    let results = state.db.find_similar_documents(
        "default",
        &doc_id,
        req.limit,
    ).map_err(|e| ApiError::internal(format!("Failed to find similar: {}", e)))?;

    let search_results: Vec<DocumentSearchResult> = results
        .into_iter()
        .map(|(doc, score)| {
            // Convert metadata
            let metadata = match doc.metadata {
                Some(serde_json::Value::Object(map)) => map.into_iter().collect(),
                _ => HashMap::new(),
            };

            DocumentSearchResult {
                id: doc.id,
                score,
                content: if req.include_content { Some(doc.content) } else { None },
                highlights: None,
                metadata,
                chunks: None,
            }
        })
        .collect();

    Ok(Json(ApiResponse::success(search_results)))
}
