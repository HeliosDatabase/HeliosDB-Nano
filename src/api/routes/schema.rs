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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// v10.2: /schema/compare used to answer every request with no
    /// differences and `is_compatible: true` without comparing anything.
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
    async fn schema_compare_answers_501() {
        let state = AppState {
            db: Arc::new(EmbeddedDatabase::new_in_memory().unwrap()),
            query_registry: Arc::new(crate::compute::QueryRegistry::new()),
            auth_bridge: None,
            oauth_registry: None,
            change_notifier: None,
        };
        let router: axum::Router<()> = routes().with_state(state);
        let response = router
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/compare")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source":"CREATE TABLE a (x INT)","target":"CREATE TABLE b (y TEXT)"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("No changes detected"), "{text}");
        assert!(text.contains("not implemented"), "{text}");
    }
}
