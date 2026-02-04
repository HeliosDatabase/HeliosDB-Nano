//! Vector store and search API handlers
//!
//! Provides REST API endpoints for vector operations including:
//! - Vector store management (create, delete, list)
//! - Vector insertion and upsert
//! - Similarity search (k-NN, filtered, hybrid)
//! - Text-to-vector with automatic embedding

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

/// Vector store configuration
#[derive(Debug, Deserialize)]
pub struct CreateVectorStoreRequest {
    /// Store name
    pub name: String,
    /// Vector dimensions
    pub dimensions: usize,
    /// Distance metric (cosine, euclidean, dot)
    #[serde(default = "default_metric")]
    pub metric: String,
    /// Index type (hnsw, flat, ivf)
    #[serde(default = "default_index_type")]
    pub index_type: String,
    /// HNSW M parameter (connections per layer)
    #[serde(default = "default_hnsw_m")]
    pub hnsw_m: usize,
    /// HNSW ef_construction parameter
    #[serde(default = "default_hnsw_ef")]
    pub hnsw_ef_construction: usize,
}

fn default_metric() -> String {
    "cosine".to_string()
}

fn default_index_type() -> String {
    "hnsw".to_string()
}

fn default_hnsw_m() -> usize {
    16
}

fn default_hnsw_ef() -> usize {
    200
}

/// Vector store info response
#[derive(Debug, Serialize)]
pub struct VectorStoreInfo {
    pub name: String,
    pub dimensions: usize,
    pub metric: String,
    pub index_type: String,
    pub vector_count: usize,
    pub created_at: String,
}

/// Vector insertion request
#[derive(Debug, Deserialize)]
pub struct InsertVectorsRequest {
    /// Vector IDs (optional, auto-generated if not provided)
    pub ids: Option<Vec<String>>,
    /// Vector data
    pub vectors: Vec<Vec<f32>>,
    /// Metadata for each vector
    pub metadata: Option<Vec<HashMap<String, serde_json::Value>>>,
    /// Namespace for organization
    pub namespace: Option<String>,
}

/// Vector upsert request
#[derive(Debug, Deserialize)]
pub struct UpsertVectorsRequest {
    /// Vector entries with IDs
    pub vectors: Vec<VectorEntry>,
    /// Namespace for organization
    pub namespace: Option<String>,
}

/// Single vector entry
#[derive(Debug, Deserialize, Serialize)]
pub struct VectorEntry {
    /// Vector ID
    pub id: String,
    /// Vector values
    pub values: Vec<f32>,
    /// Optional metadata
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Vector search request
#[derive(Debug, Deserialize)]
pub struct SearchVectorsRequest {
    /// Query vector
    pub vector: Vec<f32>,
    /// Number of results to return
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Minimum similarity threshold (0.0-1.0)
    pub min_score: Option<f32>,
    /// Metadata filter
    pub filter: Option<HashMap<String, serde_json::Value>>,
    /// Include vector values in response
    #[serde(default)]
    pub include_values: bool,
    /// Include metadata in response
    #[serde(default = "default_true")]
    pub include_metadata: bool,
    /// Namespace to search in
    pub namespace: Option<String>,
}

fn default_top_k() -> usize {
    10
}

fn default_true() -> bool {
    true
}

/// Search result entry
#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Search response
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub query_time_ms: u64,
    pub namespace: Option<String>,
}

/// Text search request (uses embedding provider)
#[derive(Debug, Deserialize)]
pub struct TextSearchRequest {
    /// Text query to embed and search
    pub text: String,
    /// Number of results
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Metadata filter
    pub filter: Option<HashMap<String, serde_json::Value>>,
    /// Include metadata
    #[serde(default = "default_true")]
    pub include_metadata: bool,
    /// Namespace
    pub namespace: Option<String>,
}

/// Store texts with automatic embedding
#[derive(Debug, Deserialize)]
pub struct StoreTextsRequest {
    /// Text content to embed
    pub texts: Vec<String>,
    /// Optional IDs (auto-generated if not provided)
    pub ids: Option<Vec<String>>,
    /// Metadata for each text
    pub metadatas: Option<Vec<HashMap<String, serde_json::Value>>>,
    /// Namespace
    pub namespace: Option<String>,
}

/// Hybrid search request (vector + keyword)
#[derive(Debug, Deserialize)]
pub struct HybridSearchRequest {
    /// Text query for keyword search
    pub text: Option<String>,
    /// Vector query for semantic search
    pub vector: Option<Vec<f32>>,
    /// Number of results
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Weight for semantic vs keyword (0.0 = all keyword, 1.0 = all semantic)
    #[serde(default = "default_alpha")]
    pub alpha: f32,
    /// Metadata filter
    pub filter: Option<HashMap<String, serde_json::Value>>,
    /// Namespace
    pub namespace: Option<String>,
}

fn default_alpha() -> f32 {
    0.5
}

/// Delete vectors request
#[derive(Debug, Deserialize)]
pub struct DeleteVectorsRequest {
    /// Vector IDs to delete
    pub ids: Option<Vec<String>>,
    /// Delete by filter
    pub filter: Option<HashMap<String, serde_json::Value>>,
    /// Delete all in namespace
    pub delete_all: Option<bool>,
    /// Namespace
    pub namespace: Option<String>,
}

/// Query parameters for listing stores
#[derive(Debug, Deserialize)]
pub struct ListStoresQuery {
    /// Filter by name pattern
    pub pattern: Option<String>,
    /// Maximum results
    pub limit: Option<usize>,
}

// ============================================================================
// Handler implementations
// ============================================================================

/// List all vector stores
pub async fn list_stores(
    State(state): State<AppState>,
    Query(query): Query<ListStoresQuery>,
) -> Result<Json<ApiResponse<Vec<VectorStoreInfo>>>, ApiError> {
    let stores = state.db.list_vector_stores()
        .map_err(|e| ApiError::internal(format!("Failed to list stores: {}", e)))?;

    let mut store_infos: Vec<VectorStoreInfo> = stores
        .into_iter()
        .filter(|s| {
            if let Some(ref pattern) = query.pattern {
                s.name.contains(pattern)
            } else {
                true
            }
        })
        .map(|s| VectorStoreInfo {
            name: s.name,
            dimensions: s.dimensions as usize,
            metric: s.metric,
            index_type: s.index_type,
            vector_count: s.vector_count as usize,
            created_at: s.created_at,
        })
        .collect();

    if let Some(limit) = query.limit {
        store_infos.truncate(limit);
    }

    Ok(Json(ApiResponse::success(store_infos)))
}

/// Create a new vector store
pub async fn create_store(
    State(state): State<AppState>,
    Json(req): Json<CreateVectorStoreRequest>,
) -> Result<(StatusCode, Json<ApiResponse<VectorStoreInfo>>), ApiError> {
    // Validate dimensions
    if req.dimensions == 0 || req.dimensions > 65536 {
        return Err(ApiError::bad_request("Dimensions must be between 1 and 65536"));
    }

    let store = state.db.create_vector_store(
        &req.name,
        req.dimensions as u32,
    ).map_err(|e| ApiError::internal(format!("Failed to create store: {}", e)))?;

    let info = VectorStoreInfo {
        name: store.name,
        dimensions: store.dimensions as usize,
        metric: store.metric,
        index_type: store.index_type,
        vector_count: 0,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    Ok((StatusCode::CREATED, Json(ApiResponse::success(info))))
}

/// Get vector store info
pub async fn get_store(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
) -> Result<Json<ApiResponse<VectorStoreInfo>>, ApiError> {
    let store = state.db.get_vector_store(&store_name)
        .map_err(|e| ApiError::not_found(format!("Store not found: {}", e)))?;

    let info = VectorStoreInfo {
        name: store.name,
        dimensions: store.dimensions as usize,
        metric: store.metric,
        index_type: store.index_type,
        vector_count: store.vector_count as usize,
        created_at: store.created_at,
    };

    Ok(Json(ApiResponse::success(info)))
}

/// Delete a vector store
pub async fn delete_store(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.db.delete_vector_store(&store_name)
        .map_err(|e| ApiError::internal(format!("Failed to delete store: {}", e)))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Insert vectors into a store
pub async fn insert_vectors(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<InsertVectorsRequest>,
) -> Result<(StatusCode, Json<ApiResponse<serde_json::Value>>), ApiError> {
    // Generate IDs if not provided
    let ids = req.ids.unwrap_or_else(|| {
        req.vectors.iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect()
    });

    if ids.len() != req.vectors.len() {
        return Err(ApiError::bad_request("Number of IDs must match number of vectors"));
    }

    let _ids = state.db.insert_vectors(
        &store_name,
        req.vectors,
    ).map_err(|e| ApiError::internal(format!("Failed to insert vectors: {}", e)))?;
    let count = ids.len();

    Ok((StatusCode::CREATED, Json(ApiResponse::success(serde_json::json!({
        "inserted_count": count,
        "ids": ids,
    })))))
}

/// Upsert vectors into a store
pub async fn upsert_vectors(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<UpsertVectorsRequest>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let ids: Vec<String> = req.vectors.iter().map(|v| v.id.clone()).collect();
    let values: Vec<Vec<f32>> = req.vectors.iter().map(|v| v.values.clone()).collect();
    let metadata: Option<Vec<HashMap<String, serde_json::Value>>> =
        if req.vectors.iter().any(|v| v.metadata.is_some()) {
            Some(req.vectors.iter().map(|v| v.metadata.clone().unwrap_or_default()).collect())
        } else {
            None
        };

    let vectors_with_ids: Vec<(String, Vec<f32>)> = ids.into_iter().zip(values.into_iter()).collect();
    state.db.upsert_vectors(
        &store_name,
        vectors_with_ids,
    ).map_err(|e| ApiError::internal(format!("Failed to upsert vectors: {}", e)))?;
    let count = req.vectors.len();

    Ok(Json(ApiResponse::success(serde_json::json!({
        "upserted_count": count,
    }))))
}

/// Search vectors by similarity
pub async fn search_vectors(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<SearchVectorsRequest>,
) -> Result<Json<ApiResponse<SearchResponse>>, ApiError> {
    let start = std::time::Instant::now();

    let raw_results = state.db.search_vectors(
        &store_name,
        req.vector.clone(),
        req.top_k,
    ).map_err(|e| ApiError::internal(format!("Search failed: {}", e)))?;

    let results: Vec<_> = raw_results.into_iter().map(|(id, score)| {
        crate::api::models::VectorSearchResult {
            id,
            score,
            values: None,
            metadata: None,
        }
    }).collect();

    let search_results: Vec<SearchResult> = results
        .into_iter()
        .map(|r| SearchResult {
            id: r.id,
            score: r.score,
            values: if req.include_values { r.values } else { None },
            metadata: if req.include_metadata {
                r.metadata.and_then(|v| {
                    if let serde_json::Value::Object(map) = v {
                        Some(map.into_iter().collect())
                    } else {
                        None
                    }
                })
            } else {
                None
            },
        })
        .collect();

    Ok(Json(ApiResponse::success(SearchResponse {
        results: search_results,
        query_time_ms: start.elapsed().as_millis() as u64,
        namespace: req.namespace,
    })))
}

/// Search by text (auto-embed query)
pub async fn text_search(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<TextSearchRequest>,
) -> Result<Json<ApiResponse<SearchResponse>>, ApiError> {
    let start = std::time::Instant::now();

    let raw_results = state.db.text_search(
        &req.text,
    ).map_err(|e| ApiError::internal(format!("Text search failed: {}", e)))?;

    let results: Vec<_> = raw_results.into_iter().map(|id| {
        crate::api::models::VectorSearchResult {
            id,
            score: 0.0,
            values: None,
            metadata: None,
        }
    }).collect();

    let search_results: Vec<SearchResult> = results
        .into_iter()
        .map(|r| SearchResult {
            id: r.id,
            score: r.score,
            values: None,
            metadata: if req.include_metadata {
                r.metadata.and_then(|v| {
                    if let serde_json::Value::Object(map) = v {
                        Some(map.into_iter().collect())
                    } else {
                        None
                    }
                })
            } else {
                None
            },
        })
        .collect();

    Ok(Json(ApiResponse::success(SearchResponse {
        results: search_results,
        query_time_ms: start.elapsed().as_millis() as u64,
        namespace: req.namespace,
    })))
}

/// Store texts with automatic embedding
pub async fn store_texts(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<StoreTextsRequest>,
) -> Result<(StatusCode, Json<ApiResponse<serde_json::Value>>), ApiError> {
    let ids = req.ids.unwrap_or_else(|| {
        req.texts.iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect()
    });

    if ids.len() != req.texts.len() {
        return Err(ApiError::bad_request("Number of IDs must match number of texts"));
    }

    let _stored_ids = state.db.store_texts(
        &store_name,
        req.texts.clone(),
    ).map_err(|e| ApiError::internal(format!("Failed to store texts: {}", e)))?;
    let count = ids.len();

    Ok((StatusCode::CREATED, Json(ApiResponse::success(serde_json::json!({
        "stored_count": count,
        "ids": ids,
    })))))
}

/// Hybrid search (vector + keyword)
pub async fn hybrid_search(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<HybridSearchRequest>,
) -> Result<Json<ApiResponse<SearchResponse>>, ApiError> {
    let start = std::time::Instant::now();

    if req.text.is_none() && req.vector.is_none() {
        return Err(ApiError::bad_request("Either text or vector must be provided"));
    }

    let raw_results = state.db.hybrid_search(
        &store_name,
        req.text.as_deref().unwrap_or(""),
        req.top_k,
    ).map_err(|e| ApiError::internal(format!("Hybrid search failed: {}", e)))?;

    let results: Vec<_> = raw_results.into_iter().map(|(id, score)| {
        crate::api::models::VectorSearchResult {
            id,
            score,
            values: None,
            metadata: None,
        }
    }).collect();

    let search_results: Vec<SearchResult> = results
        .into_iter()
        .map(|r| SearchResult {
            id: r.id,
            score: r.score,
            values: None,
            metadata: r.metadata.and_then(|v| {
                if let serde_json::Value::Object(map) = v {
                    Some(map.into_iter().collect())
                } else {
                    None
                }
            }),
        })
        .collect();

    Ok(Json(ApiResponse::success(SearchResponse {
        results: search_results,
        query_time_ms: start.elapsed().as_millis() as u64,
        namespace: req.namespace,
    })))
}

/// Delete vectors
pub async fn delete_vectors(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<DeleteVectorsRequest>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    state.db.delete_vectors(
        &store_name,
        req.ids.clone().unwrap_or_default(),
    ).map_err(|e| ApiError::internal(format!("Failed to delete vectors: {}", e)))?;
    let count = req.ids.as_ref().map(|v| v.len()).unwrap_or(0);

    Ok(Json(ApiResponse::success(serde_json::json!({
        "deleted_count": count,
    }))))
}

/// Fetch vectors by ID
pub async fn fetch_vectors(
    State(state): State<AppState>,
    Path((store_name, ids)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<ApiResponse<Vec<VectorEntry>>>, ApiError> {
    let id_list: Vec<&str> = ids.split(',').collect();
    let namespace = params.get("namespace").map(|s| s.as_str());

    let id_strings: Vec<String> = id_list.iter().map(|s| s.to_string()).collect();
    let vectors = state.db.fetch_vectors(
        &store_name,
        id_strings,
    ).map_err(|e| ApiError::internal(format!("Failed to fetch vectors: {}", e)))?;

    let entries: Vec<VectorEntry> = vectors
        .into_iter()
        .map(|(id, values)| VectorEntry {
            id,
            values,
            metadata: None,
        })
        .collect();

    Ok(Json(ApiResponse::success(entries)))
}
