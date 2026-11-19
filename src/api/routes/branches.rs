//! Branch management routes
//!
//! Provides REST endpoints for CRUD operations on database branches.

use axum::{
    routing::{delete, get, post},
    Router,
};

use crate::api::{handlers::branch_handler, server::AppState};

/// Create branch management routes
///
/// # Endpoints
///
/// - `GET /v1/branches` - List all branches
/// - `POST /v1/branches` - Create a new branch
/// - `GET /v1/branches/:name` - Get branch details
/// - `DELETE /v1/branches/:name` - Delete a branch
/// - `POST /v1/branches/:name/merge` - Merge branches
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(branch_handler::list_branches))
        .route("/", post(branch_handler::create_branch))
        .route("/:name", get(branch_handler::get_branch))
        .route("/:name", delete(branch_handler::delete_branch))
        .route("/:name/merge", post(branch_handler::merge_branch))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;
    use std::sync::Arc;

    #[test]
    fn test_routes_creation() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(crate::compute::QueryRegistry::new());
        let state = AppState {
            db,
            query_registry,
            auth_bridge: None,
            oauth_registry: None,
            change_notifier: None,
        };
        let router: axum::Router<()> = routes().with_state(state);
        // Router created successfully
        drop(router);
    }

    /// v10.2: the merge never looks for conflicts, so its message must not
    /// claim there were none.
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
    async fn merge_message_does_not_claim_no_conflicts() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE BRANCH dev AS OF NOW").unwrap();
        db.execute("USE BRANCH dev").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("USE BRANCH main").unwrap();
        let state = AppState {
            db,
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
                    .uri("/dev/merge")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"target":"main"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(status.is_success(), "{status}: {text}");
        assert!(!text.contains("no conflicts"), "{text}");
        assert!(text.contains("conflicts are not detected"), "{text}");
    }
}
