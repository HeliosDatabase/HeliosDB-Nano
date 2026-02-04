//! Vector API routes
//!
//! Routes for vector store operations, similarity search, and embeddings.

use axum::{
    routing::{delete, get, post},
    Router,
};

use crate::api::handlers::vector_handler;
use crate::api::server::AppState;

/// Create vector routes
pub fn routes() -> Router<AppState> {
    Router::new()
        // Store management
        .route("/stores", get(vector_handler::list_stores))
        .route("/stores", post(vector_handler::create_store))
        .route("/stores/:store_name", get(vector_handler::get_store))
        .route("/stores/:store_name", delete(vector_handler::delete_store))
        // Vector operations
        .route("/stores/:store_name/vectors", post(vector_handler::insert_vectors))
        .route("/stores/:store_name/upsert", post(vector_handler::upsert_vectors))
        .route("/stores/:store_name/vectors/:ids", get(vector_handler::fetch_vectors))
        .route("/stores/:store_name/delete", post(vector_handler::delete_vectors))
        // Search operations
        .route("/stores/:store_name/search", post(vector_handler::search_vectors))
        .route("/stores/:store_name/search/text", post(vector_handler::text_search))
        .route("/stores/:store_name/search/hybrid", post(vector_handler::hybrid_search))
        // Text operations (auto-embedding)
        .route("/stores/:store_name/texts", post(vector_handler::store_texts))
}
