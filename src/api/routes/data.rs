//! Data operation routes
//!
//! Provides REST endpoints for CRUD operations on table data.

use axum::{
    Router,
    routing::get,
};

use crate::api::{
    handlers::data_handler,
    server::AppState,
};

/// Create data operation routes
///
/// # Endpoints
///
/// - `GET /v1/branches/:name/tables` - List all tables in a branch
/// - `GET /v1/branches/:name/tables/:table/data` - Query table data with pagination and filtering
/// - `POST /v1/branches/:name/tables/:table/data` - Insert data into a table
/// - `PUT /v1/branches/:name/tables/:table/data` - Update data in a table
/// - `DELETE /v1/branches/:name/tables/:table/data` - Delete data from a table
///
/// # Query Parameters (for GET)
///
/// - `filter`: WHERE clause filter (optional)
/// - `columns`: Comma-separated column names (optional, defaults to *)
/// - `page`: Page number, 1-based (optional, default: 1)
/// - `limit`: Page size (optional, default: 100, max: 1000)
/// - `as_of`: Time-travel timestamp (optional)
/// - `order_by`: ORDER BY clause (optional)
///
/// # Examples
///
/// ```bash
/// # List all tables in main branch
/// curl http://localhost:8080/v1/branches/main/tables
///
/// # Query all data from a table
/// curl http://localhost:8080/v1/branches/main/tables/users/data
///
/// # Query with filter and pagination
/// curl "http://localhost:8080/v1/branches/main/tables/users/data?filter=age>25&page=1&limit=10"
///
/// # Query specific columns
/// curl "http://localhost:8080/v1/branches/main/tables/users/data?columns=id,name,email"
///
/// # Time-travel query (as of timestamp)
/// curl "http://localhost:8080/v1/branches/main/tables/users/data?as_of=1234567890"
///
/// # Insert data
/// curl -X POST http://localhost:8080/v1/branches/main/tables/users/data \
///   -H "Content-Type: application/json" \
///   -d '{"rows": [{"id": 1, "name": "Alice", "email": "alice@example.com"}]}'
///
/// # Update data
/// curl -X PUT http://localhost:8080/v1/branches/main/tables/users/data \
///   -H "Content-Type: application/json" \
///   -d '{"values": {"email": "newemail@example.com"}, "filter": "id = 1"}'
///
/// # Delete data
/// curl -X DELETE http://localhost:8080/v1/branches/main/tables/users/data \
///   -H "Content-Type: application/json" \
///   -d '{"filter": "id = 1"}'
/// ```
pub fn routes() -> Router<AppState> {
    Router::new()
        // List tables in branch
        .route("/:name/tables", get(data_handler::list_tables))
        // Query, insert, update, delete data
        .route(
            "/:name/tables/:table/data",
            get(data_handler::query_data)
                .post(data_handler::insert_data)
                .put(data_handler::update_data)
                .delete(data_handler::delete_data),
        )
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
        let state = AppState { db, query_registry, auth_bridge: None, oauth_registry: None, change_notifier: None };
        let router: axum::Router<()> = routes().with_state(state);
        // Router created successfully
        drop(router);
    }
}
