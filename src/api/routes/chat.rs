//! Chat completion API routes
//!
//! OpenAI-compatible chat completion routes with RAG support.

use axum::{
    routing::{get, post},
    Router,
};

use crate::api::handlers::chat_handler;
use crate::api::server::AppState;

/// Create chat routes
pub fn routes() -> Router<AppState> {
    Router::new()
        // Chat completions (OpenAI-compatible)
        .route("/completions", post(chat_handler::create_chat_completion))
        // Models
        .route("/models", get(chat_handler::list_models))
        .route("/models/:model_id", get(chat_handler::get_model))
        // Embeddings (OpenAI-compatible)
        .route("/embeddings", post(chat_handler::create_embeddings))
}
