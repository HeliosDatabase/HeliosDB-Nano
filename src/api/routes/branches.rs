//! Branch management routes
//!
//! Provides REST endpoints for CRUD operations on database branches.

use axum::{
    Router,
    routing::{get, post, delete},
};

use crate::api::{
    handlers::branch_handler,
    server::AppState,
};

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
        let state = AppState { db, query_registry };
        let router: axum::Router<()> = routes().with_state(state);
        // Router created successfully
        drop(router);
    }
}
