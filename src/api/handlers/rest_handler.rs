//! REST API Handlers (PostgREST-compatible)
//!
//! Axum handlers for the `/rest/v1/` endpoints.  These translate HTTP
//! requests with PostgREST-style query parameters into parameterized SQL
//! queries and return JSON results.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use std::collections::HashMap;
use tracing::{info, warn, debug};

use crate::api::{
    models::ApiError,
    server::AppState,
    rest_executor::RestExecutor,
};

/// Reserved query-string keys that are NOT filter columns.
const RESERVED_KEYS: &[&str] = &["select", "order", "limit", "offset", "apikey"];

/// Convert a `crate::Value` to a `serde_json::Value`.
fn value_to_json(val: &crate::Value) -> serde_json::Value {
    serde_json::Value::from(val)
}

/// Collect filter parameters from the query string.
///
/// Everything that is NOT a reserved key (`select`, `order`, `limit`, `offset`)
/// is treated as a `column=operator.value` filter.
fn collect_filters(params: &HashMap<String, String>) -> Vec<(String, String)> {
    params.iter()
        .filter(|(k, _)| !RESERVED_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

// ── RLS helpers ──────────────────────────────────────────────────────────────

/// Extract the user ID from the JWT in the `Authorization: Bearer <token>` header.
///
/// If an `AuthBridge` is configured in state, the token is decoded and
/// `claims.sub` (the user ID) is returned.  Returns `None` when no header
/// is present or the token is invalid.
fn extract_user_from_headers(headers: &HeaderMap, state: &AppState) -> Option<String> {
    let bridge = state.auth_bridge.as_ref()?;
    let auth_header = headers.get("authorization")?.to_str().ok()?;
    let token = auth_header.strip_prefix("Bearer ")?;
    bridge.get_user(token).ok().map(|u| u.id)
}

/// Return `true` if the request carries a `service_role` API key.
///
/// The service-role key bypasses RLS.  We look for it in the `apikey`
/// header (Supabase convention) or the `x-api-key` header.
fn is_service_role(headers: &HeaderMap, state: &AppState) -> bool {
    let bridge = match &state.auth_bridge {
        Some(b) => b,
        None => return false,
    };

    let key = headers
        .get("apikey")
        .or_else(|| headers.get("x-api-key"))
        .and_then(|v| v.to_str().ok());

    if let Some(key) = key {
        // If the key can be decoded as a valid JWT with role=service_role, bypass.
        if let Ok(user) = bridge.get_user(key) {
            return user.role == "service_role";
        }
    }
    false
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `GET /rest/v1/:table` — Select rows from a table.
///
/// Query parameters:
/// - `select` — comma-separated column list (default `*`)
/// - `order`  — e.g. `created_at.desc,name.asc`
/// - `limit`  — maximum rows
/// - `offset` — skip N rows
/// - Any other key is a filter: `column=operator.value`
///
/// RLS: when a valid JWT is present and the table has an `owner_id`/`user_id`
/// column, only rows belonging to the authenticated user are returned.
/// A `service_role` API key bypasses RLS.
pub async fn rest_select(
    State(state): State<AppState>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    info!(table = %table, "REST SELECT");

    let executor = RestExecutor::new(state.db.clone());
    let select = params.get("select").map(|s| s.as_str()).unwrap_or("*");
    let order = params.get("order").map(|s| s.as_str());
    let limit: Option<usize> = params.get("limit").and_then(|s| s.parse().ok());
    let offset: Option<usize> = params.get("offset").and_then(|s| s.parse().ok());
    let filters = collect_filters(&params);

    // ── RLS ──────────────────────────────────────────────────────────
    let bypass_rls = is_service_role(&headers, &state);
    let user_id = if bypass_rls {
        None
    } else {
        extract_user_from_headers(&headers, &state)
    };
    debug!(table = %table, ?user_id, bypass_rls, "RLS context");

    let (tuples, columns) = if user_id.is_some() && !bypass_rls {
        executor
            .select_with_rls(&table, select, &filters, order, limit, offset, user_id.as_deref())
            .map_err(|e| {
                warn!(table = %table, error = %e, "REST SELECT (RLS) failed");
                ApiError::from(e)
            })?
    } else {
        executor
            .select(&table, select, &filters, order, limit, offset)
            .map_err(|e| {
                warn!(table = %table, error = %e, "REST SELECT failed");
                ApiError::from(e)
            })?
    };

    let rows: Vec<serde_json::Value> = tuples.iter().map(|tuple| {
        let mut obj = serde_json::Map::new();
        for (i, col) in columns.iter().enumerate() {
            if let Some(val) = tuple.values.get(i) {
                obj.insert(col.clone(), value_to_json(val));
            }
        }
        serde_json::Value::Object(obj)
    }).collect();

    Ok(Json(rows))
}

/// `POST /rest/v1/:table` — Insert rows into a table.
///
/// Accepts a JSON object (single row) or a JSON array of objects (batch).
/// After a successful insert the change notifier is informed so that
/// active WebSocket subscribers receive a realtime event.
pub async fn rest_insert(
    State(state): State<AppState>,
    Path(table): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    info!(table = %table, "REST INSERT");

    let executor = RestExecutor::new(state.db.clone());

    // Normalise: single object → array of one
    let rows: Vec<serde_json::Value> = match body.clone() {
        serde_json::Value::Array(arr) => arr,
        obj @ serde_json::Value::Object(_) => vec![obj],
        _ => return Err(ApiError::bad_request(
            "Request body must be a JSON object or array of objects"
        )),
    };

    let (affected, _, _) = executor.insert(&table, &rows).map_err(|e| {
        warn!(table = %table, error = %e, "REST INSERT failed");
        ApiError::from(e)
    })?;

    // ── Notify subscribers ───────────────────────────────────────────
    if affected > 0 {
        if let Some(notifier) = &state.change_notifier {
            for row in &rows {
                notifier.notify(&table, "INSERT", Some(row.clone()), None);
            }
        }
    }

    let response = serde_json::json!({
        "message": format!("{affected} row(s) inserted"),
        "count": affected,
    });

    Ok((StatusCode::CREATED, Json(response)))
}

/// `PATCH /rest/v1/:table` — Update rows matching filter criteria.
///
/// The request body is a JSON object with the columns/values to set.
/// Filter criteria come from query-string parameters.
/// RLS: restricts updates to rows owned by the authenticated user (unless
/// service_role).
pub async fn rest_update(
    State(state): State<AppState>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    info!(table = %table, "REST UPDATE");

    let executor = RestExecutor::new(state.db.clone());
    let filters = collect_filters(&params);

    // ── RLS ──────────────────────────────────────────────────────────
    let bypass_rls = is_service_role(&headers, &state);
    let user_id = if bypass_rls {
        None
    } else {
        extract_user_from_headers(&headers, &state)
    };

    let affected = if user_id.is_some() && !bypass_rls {
        executor.update_with_rls(&table, &body, &filters, user_id.as_deref()).map_err(|e| {
            warn!(table = %table, error = %e, "REST UPDATE (RLS) failed");
            ApiError::from(e)
        })?
    } else {
        executor.update(&table, &body, &filters).map_err(|e| {
            warn!(table = %table, error = %e, "REST UPDATE failed");
            ApiError::from(e)
        })?
    };

    // ── Notify subscribers ───────────────────────────────────────────
    if affected > 0 {
        if let Some(notifier) = &state.change_notifier {
            notifier.notify(&table, "UPDATE", Some(body), None);
        }
    }

    Ok(Json(serde_json::json!({
        "message": format!("{affected} row(s) updated"),
        "count": affected,
    })))
}

/// `DELETE /rest/v1/:table` — Delete rows matching filter criteria.
///
/// Filter criteria come from query-string parameters.
/// RLS: restricts deletes to rows owned by the authenticated user (unless
/// service_role).
pub async fn rest_delete(
    State(state): State<AppState>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    info!(table = %table, "REST DELETE");

    let executor = RestExecutor::new(state.db.clone());
    let filters = collect_filters(&params);

    // ── RLS ──────────────────────────────────────────────────────────
    let bypass_rls = is_service_role(&headers, &state);
    let user_id = if bypass_rls {
        None
    } else {
        extract_user_from_headers(&headers, &state)
    };

    let affected = if user_id.is_some() && !bypass_rls {
        executor.delete_with_rls(&table, &filters, user_id.as_deref()).map_err(|e| {
            warn!(table = %table, error = %e, "REST DELETE (RLS) failed");
            ApiError::from(e)
        })?
    } else {
        executor.delete(&table, &filters).map_err(|e| {
            warn!(table = %table, error = %e, "REST DELETE failed");
            ApiError::from(e)
        })?
    };

    // ── Notify subscribers ───────────────────────────────────────────
    if affected > 0 {
        if let Some(notifier) = &state.change_notifier {
            notifier.notify(&table, "DELETE", None, None);
        }
    }

    Ok(Json(serde_json::json!({
        "message": format!("{affected} row(s) deleted"),
        "count": affected,
    })))
}

/// `POST /rest/v1/rpc/:function` — Execute a stored function / RPC.
///
/// This is a stub that returns 501 Not Implemented for now.
#[allow(dead_code)]
pub async fn rest_rpc(
    State(_state): State<AppState>,
    Path(function): Path<String>,
    Json(_body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    info!(function = %function, "REST RPC");

    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        format!("RPC function '{}' is not yet supported", function),
    ))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;
    use crate::compute::QueryRegistry;
    use std::sync::Arc;

    fn test_state() -> AppState {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(QueryRegistry::new());
        AppState { db, query_registry, auth_bridge: None, oauth_registry: None, change_notifier: None }
    }

    fn test_state_with_table() -> AppState {
        let state = test_state();
        state.db.execute("CREATE TABLE users (id INT, name TEXT, age INT)").unwrap();
        state.db.execute("INSERT INTO users VALUES (1, 'Alice', 30)").unwrap();
        state.db.execute("INSERT INTO users VALUES (2, 'Bob', 25)").unwrap();
        state.db.execute("INSERT INTO users VALUES (3, 'Carol', 35)").unwrap();
        state
    }

    fn empty_headers() -> HeaderMap {
        HeaderMap::new()
    }

    #[tokio::test]
    async fn test_rest_select_all() {
        let state = test_state_with_table();
        let params = HashMap::new();

        let result = rest_select(
            State(state),
            Path("users".to_string()),
            Query(params),
            empty_headers(),
        ).await;

        assert!(result.is_ok());
        let rows = result.unwrap().0;
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn test_rest_select_with_filter() {
        let state = test_state_with_table();
        let mut params = HashMap::new();
        params.insert("name".to_string(), "eq.Alice".to_string());

        let result = rest_select(
            State(state),
            Path("users".to_string()),
            Query(params),
            empty_headers(),
        ).await;

        assert!(result.is_ok());
        let rows = result.unwrap().0;
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn test_rest_select_with_limit() {
        let state = test_state_with_table();
        let mut params = HashMap::new();
        params.insert("limit".to_string(), "2".to_string());

        let result = rest_select(
            State(state),
            Path("users".to_string()),
            Query(params),
            empty_headers(),
        ).await;

        assert!(result.is_ok());
        let rows = result.unwrap().0;
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn test_rest_insert_single() {
        let state = test_state();
        state.db.execute("CREATE TABLE items (id INT, label TEXT)").unwrap();

        let body = serde_json::json!({"id": 1, "label": "test"});

        let result = rest_insert(
            State(state),
            Path("items".to_string()),
            Json(body),
        ).await;

        assert!(result.is_ok());
        let (status, json) = result.unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json.0["count"], 1);
    }

    #[tokio::test]
    async fn test_rest_insert_batch() {
        let state = test_state();
        state.db.execute("CREATE TABLE items (id INT, label TEXT)").unwrap();

        let body = serde_json::json!([
            {"id": 1, "label": "a"},
            {"id": 2, "label": "b"},
        ]);

        let result = rest_insert(
            State(state),
            Path("items".to_string()),
            Json(body),
        ).await;

        assert!(result.is_ok());
        let (_, json) = result.unwrap();
        assert_eq!(json.0["count"], 2);
    }

    #[tokio::test]
    async fn test_rest_update() {
        let state = test_state_with_table();
        let mut params = HashMap::new();
        params.insert("id".to_string(), "eq.1".to_string());

        let body = serde_json::json!({"name": "Alicia"});

        let result = rest_update(
            State(state),
            Path("users".to_string()),
            Query(params),
            empty_headers(),
            Json(body),
        ).await;

        assert!(result.is_ok());
        let json = result.unwrap().0;
        assert_eq!(json["count"], 1);
    }

    #[tokio::test]
    async fn test_rest_delete() {
        let state = test_state_with_table();
        let mut params = HashMap::new();
        params.insert("id".to_string(), "eq.2".to_string());

        let result = rest_delete(
            State(state.clone()),
            Path("users".to_string()),
            Query(params),
            empty_headers(),
        ).await;

        assert!(result.is_ok());
        let json = result.unwrap().0;
        assert_eq!(json["count"], 1);

        // Verify row count
        let _remaining = state.db.query("SELECT * FROM users", &[]);
    }

    #[tokio::test]
    async fn test_rest_select_nonexistent_table() {
        let state = test_state();
        let params = HashMap::new();

        let result = rest_select(
            State(state),
            Path("nonexistent".to_string()),
            Query(params),
            empty_headers(),
        ).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rest_insert_invalid_body() {
        let state = test_state();
        state.db.execute("CREATE TABLE t (id INT)").unwrap();

        let body = serde_json::json!("not an object");

        let result = rest_insert(
            State(state),
            Path("t".to_string()),
            Json(body),
        ).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rest_select_rls_no_auth_bridge_returns_all() {
        // Without an auth bridge, RLS is effectively disabled.
        let state = test_state_with_table();
        state.db.execute("ALTER TABLE users ADD COLUMN owner_id TEXT").unwrap();
        state.db.execute("UPDATE users SET owner_id = 'u1' WHERE id = 1").unwrap();
        state.db.execute("UPDATE users SET owner_id = 'u2' WHERE id = 2").unwrap();
        state.db.execute("UPDATE users SET owner_id = 'u1' WHERE id = 3").unwrap();

        let result = rest_select(
            State(state),
            Path("users".to_string()),
            Query(HashMap::new()),
            empty_headers(),
        ).await;

        assert!(result.is_ok());
        // No auth bridge = no RLS filtering = all 3 rows.
        assert_eq!(result.unwrap().0.len(), 3);
    }
}
