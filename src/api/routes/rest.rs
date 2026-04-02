//! REST API route definitions (PostgREST-compatible)
//!
//! Maps the `/rest/v1/` URL namespace to the appropriate handler functions.

use axum::{
    Router,
    routing::{get, post},
};

use crate::api::{
    handlers::rest_handler,
    server::AppState,
};

/// Create PostgREST-compatible REST routes.
///
/// These are designed to be nested at `/rest/v1` in the top-level router.
///
/// # Endpoints
///
/// | Method | Path            | Description                |
/// |--------|-----------------|----------------------------|
/// | GET    | `/:table`       | Select rows from table     |
/// | POST   | `/:table`       | Insert rows into table     |
/// | PATCH  | `/:table`       | Update rows in table       |
/// | DELETE | `/:table`       | Delete rows from table     |
/// | POST   | `/rpc/:function`| Execute stored function    |
pub fn routes() -> Router<AppState> {
    Router::new()
        // RPC must come before the catch-all `:table` routes
        .route("/rpc/{function}", post(rest_handler::rest_rpc))
        .route(
            "/{table}",
            get(rest_handler::rest_select)
                .post(rest_handler::rest_insert)
                .patch(rest_handler::rest_update)
                .delete(rest_handler::rest_delete),
        )
}
