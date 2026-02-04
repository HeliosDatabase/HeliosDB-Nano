//! Route definitions for REST API

pub mod branches;
pub mod query;
pub mod data;
pub mod vectors;
pub mod agents;
pub mod documents;
pub mod chat;
pub mod schema;
pub mod cancellation;
pub mod webhooks;

use axum::Router;
use crate::api::server::AppState;
use crate::api::openapi;

/// Create v1 API routes
pub fn v1_routes() -> Router<AppState> {
    Router::new()
        // Core database routes
        .nest("/branches", branches::routes())
        .nest("/branches", query::routes())
        .merge(data::routes())
        // Query management and cancellation
        .nest("/queries", cancellation::routes())
        // AI-native routes
        .nest("/vectors", vectors::routes())
        .nest("/agents", agents::routes())
        .nest("/documents", documents::routes())
        .nest("/chat", chat::routes())
        .nest("/schema", schema::routes())
        // Git integration webhooks
        .nest("/webhooks", webhooks::routes())
        // OpenAPI documentation (public, no auth)
        .merge(openapi::routes())
}
