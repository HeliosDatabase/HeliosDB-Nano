//! Route definitions for REST API

pub mod agents;
pub mod branches;
pub mod cancellation;
pub mod chat;
pub mod data;
pub mod documents;
pub mod query;
pub mod rest;
pub mod schema;
pub mod vectors;
pub mod webhooks;

use crate::api::openapi;
use crate::api::server::AppState;
use axum::Router;

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
