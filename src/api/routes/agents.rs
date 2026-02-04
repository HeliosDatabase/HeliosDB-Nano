//! Agent memory API routes
//!
//! Routes for AI agent memory management, session handling, and semantic search.

use axum::{
    routing::{delete, get, post, put},
    Router,
};

use crate::api::handlers::agent_handler;
use crate::api::server::AppState;

/// Create agent routes
pub fn routes() -> Router<AppState> {
    Router::new()
        // Session management
        .route("/memory", get(agent_handler::list_sessions))
        .route("/memory", post(agent_handler::create_session))
        .route("/memory/:session_id", get(agent_handler::get_session))
        .route("/memory/:session_id", put(agent_handler::update_session))
        .route("/memory/:session_id", delete(agent_handler::delete_session))
        // Message operations
        .route("/memory/:session_id/add", post(agent_handler::add_message))
        .route("/memory/:session_id/messages", get(agent_handler::get_messages))
        .route("/memory/:session_id/messages/batch", post(agent_handler::add_messages))
        .route("/memory/:session_id/clear", post(agent_handler::clear_messages))
        // Memory operations
        .route("/memory/:session_id/search", post(agent_handler::search_memory))
        .route("/memory/:session_id/summarize", post(agent_handler::summarize_memory))
        .route("/memory/:session_id/context", post(agent_handler::get_context))
        .route("/memory/:session_id/fork", post(agent_handler::fork_session))
}
