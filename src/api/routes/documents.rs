//! Document API routes
//!
//! Routes for document management, chunking, and semantic search.

use axum::{
    routing::{delete, get, post, put},
    Router,
};

use crate::api::handlers::document_handler;
use crate::api::server::AppState;

/// Create document routes
pub fn routes() -> Router<AppState> {
    Router::new()
        // Document CRUD
        .route("/", get(document_handler::list_documents))
        .route("/", post(document_handler::create_document))
        .route("/batch", post(document_handler::batch_create_documents))
        .route("/:doc_id", get(document_handler::get_document))
        .route("/:doc_id", put(document_handler::update_document))
        .route("/:doc_id", delete(document_handler::delete_document))
        // Document operations
        .route("/:doc_id/chunks", get(document_handler::get_chunks))
        .route("/:doc_id/chunk", post(document_handler::chunk_document))
        .route("/:doc_id/similar", post(document_handler::similar_documents))
        // Search
        .route("/search", post(document_handler::search_documents))
}
