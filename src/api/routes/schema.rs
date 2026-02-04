//! Schema inference API routes
//!
//! Routes for AI-powered schema inference and optimization.

use axum::{
    routing::{get, post},
    Router,
};

use crate::api::handlers::schema_handler;
use crate::api::server::AppState;

/// Create schema routes
pub fn routes() -> Router<AppState> {
    Router::new()
        // Schema inference
        .route("/infer", post(schema_handler::infer_schema))
        .route("/infer/batch", post(schema_handler::batch_infer_schema))
        .route("/infer/file", post(schema_handler::infer_from_file))
        // Schema operations
        .route("/optimize", post(schema_handler::optimize_schema))
        .route("/compare", post(schema_handler::compare_schemas))
        .route("/validate", post(schema_handler::validate_schema))
        // Natural language
        .route("/generate", post(schema_handler::generate_from_description))
        // Templates
        .route("/templates", get(schema_handler::list_templates))
        .route("/templates/instantiate", post(schema_handler::instantiate_template))
}
