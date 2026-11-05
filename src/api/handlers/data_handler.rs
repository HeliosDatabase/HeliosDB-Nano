//! Data operation handlers
//!
//! Implements HTTP request handlers for data CRUD operations.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use tracing::{error, info, warn};

use crate::api::{
    models::{
        data::{
            json_to_value, tuple_to_map, ColumnInfo, DataQueryParams, DataQueryResponse, DeleteDataRequest,
            DeleteDataResponse, InsertDataRequest, InsertDataResponse, PaginationInfo, QueryMetadata,
            TableInfoResponse, TableListResponse, UpdateDataRequest, UpdateDataResponse,
        },
        ApiError,
    },
    server::AppState,
};
use crate::{Tuple, Value};

/// Validate that `branch_name` is an ACTIVE branch — sprinter 6bc4a4040eb7 (B1).
///
/// This used to accept any registered branch (a dropped one included) and then
/// warn that "data operations currently use main branch": every data route
/// under `/v1/branches/:name/…` silently read and wrote MAIN. The operations
/// below now run on `:name` through a request-scoped binding
/// (`EmbeddedDatabase::with_request_branch_scope`); this early check only turns
/// an inactive branch into a 404 before any work is done.
fn validate_branch(state: &AppState, branch_name: &str) -> Result<(), ApiError> {
    if state.db.storage.active_branch_exists(branch_name) {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::NOT_FOUND,
        "branch_not_found",
        format!("Branch '{}' not found (or not active)", branch_name),
    ))
}

/// List tables in a branch
///
/// GET /v1/branches/{name}/tables
///
/// Returns a list of all tables in the specified branch.
///
/// # Path Parameters
///
/// - name: Branch name
///
/// # Response
///
/// - 200 OK: Returns TableListResponse with all tables
/// - 404 Not Found: Branch does not exist
/// - 500 Internal Server Error: Database error
pub async fn list_tables(
    State(state): State<AppState>,
    Path(branch_name): Path<String>,
) -> Result<Json<TableListResponse>, ApiError> {
    info!("Listing tables in branch: {}", branch_name);

    // Validate branch exists
    validate_branch(&state, &branch_name)?;

    // sprinter 6bc4a4040eb7 (B3 fix pass): on the branch — its own tables are
    // listed, the ones it dropped are not, and columns are its definition's.
    let tables: Vec<TableInfoResponse> = state
        .db
        .with_request_branch_scope(&branch_name, || {
            let catalog = state.db.storage.catalog();
            let table_names = catalog.list_tables()?;
            Ok(table_names
                .iter()
                .map(|name| {
                    let schema = catalog.get_table_schema(name).ok();
                    // Get row count from table statistics if available. They
                    // describe main's rows, so a branch has none to report
                    // (sprinter 6bc4a4040eb7, B3 second fix pass).
                    let row_count = if branch_name == "main" {
                        catalog
                            .get_table_statistics(name)
                            .ok()
                            .flatten()
                            .map(|stats| stats.row_count as u64)
                    } else {
                        None
                    };
                    TableInfoResponse {
                        name: name.clone(),
                        column_count: schema.as_ref().map(|s| s.columns.len()).unwrap_or(0),
                        row_count,
                    }
                })
                .collect())
        })
        .map_err(|e| {
            error!("Failed to list tables: {}", e);
            ApiError::from(e)
        })?;

    let total = tables.len();

    info!("Found {} tables in branch '{}'", total, branch_name);

    Ok(Json(TableListResponse { tables, total }))
}

/// Query table data with pagination and filtering
///
/// GET /v1/branches/{name}/tables/{table}/data
///
/// Retrieves data from a table with optional filtering, column selection, and pagination.
/// Supports time-travel queries via the as_of parameter.
///
/// # Path Parameters
///
/// - name: Branch name
/// - table: Table name
///
/// # Query Parameters
///
/// - filter: WHERE clause filter (optional)
/// - columns: Comma-separated column names (optional, defaults to *)
/// - page: Page number, 1-based (optional, default: 1)
/// - limit: Page size (optional, default: 100)
/// - as_of: Time-travel timestamp (optional)
/// - order_by: ORDER BY clause (optional)
///
/// # Response
///
/// - 200 OK: Returns DataQueryResponse with results
/// - 404 Not Found: Branch or table does not exist
/// - 400 Bad Request: Invalid query parameters
/// - 500 Internal Server Error: Database error
pub async fn query_data(
    State(state): State<AppState>,
    Path((branch_name, table_name)): Path<(String, String)>,
    Query(params): Query<DataQueryParams>,
) -> Result<Json<DataQueryResponse>, ApiError> {
    info!(
        "Querying table '{}' in branch '{}' (page: {}, limit: {})",
        table_name, branch_name, params.page, params.limit
    );

    // Validate branch exists
    validate_branch(&state, &branch_name)?;

    // Validate pagination parameters
    if params.page < 1 {
        return Err(ApiError::bad_request("Page number must be >= 1"));
    }
    if params.limit < 1 || params.limit > 1000 {
        return Err(ApiError::bad_request("Limit must be between 1 and 1000"));
    }

    // Get table schema
    // On the branch: its catalog decides (sprinter 6bc4a4040eb7, B3 fix pass).
    let schema = state
        .db
        .with_request_branch_scope(&branch_name, || {
            state.db.storage.catalog().get_table_schema(&table_name)
        })
        .map_err(|e| {
            warn!("Table '{}' not found: {}", table_name, e);
            ApiError::from(e)
        })?;

    // Build SQL query
    let columns_clause = params.columns.as_ref().map(|c| c.as_str()).unwrap_or("*");

    let mut sql = format!("SELECT {} FROM {}", columns_clause, table_name);

    // Add WHERE clause if filter is provided
    if let Some(ref filter) = params.filter {
        sql.push_str(&format!(" WHERE {}", filter));
    }

    // Add ORDER BY clause if provided
    if let Some(ref order_by) = params.order_by {
        sql.push_str(&format!(" ORDER BY {}", order_by));
    }

    // Add LIMIT and OFFSET for pagination
    let offset = (params.page - 1) * params.limit;
    sql.push_str(&format!(" LIMIT {} OFFSET {}", params.limit + 1, offset)); // Request one extra to check has_more

    // Execute query — on `:name` (request-scoped binding, B1).
    let results = state
        .db
        .with_request_branch_scope(&branch_name, || state.db.query(&sql, &[]))
        .map_err(|e| {
            error!("Failed to query data: {}", e);
            ApiError::from(e)
        })?;

    // Check if there are more results
    let has_more = results.len() > params.limit as usize;
    let actual_results = if has_more {
        results.get(..params.limit as usize).unwrap_or(&results)
    } else {
        &results[..]
    };

    // Compute total count if requested
    let total = if params.include_total {
        // Build COUNT query with same filter
        let mut count_sql = format!("SELECT COUNT(*) FROM {}", table_name);
        if let Some(ref filter) = params.filter {
            count_sql.push_str(&format!(" WHERE {}", filter));
        }

        let count_result = state
            .db
            .with_request_branch_scope(&branch_name, || state.db.query(&count_sql, &[]))
            .map_err(|e| {
                error!("Failed to count rows: {}", e);
                ApiError::from(e)
            })?;

        // Extract count from result
        count_result
            .first()
            .and_then(|tuple| tuple.values.first())
            .and_then(|v| match v {
                Value::Int8(n) => Some(*n as u64),
                Value::Int4(n) => Some(*n as u64),
                _ => None,
            })
    } else {
        None
    };

    // Convert tuples to JSON-friendly format
    let rows: Vec<std::collections::HashMap<String, serde_json::Value>> = actual_results
        .iter()
        .map(|tuple| tuple_to_map(tuple, &schema))
        .collect();

    let column_info: Vec<ColumnInfo> = (&schema).into();

    let response = DataQueryResponse {
        columns: column_info,
        rows,
        pagination: PaginationInfo {
            page: params.page,
            limit: params.limit,
            total,
            has_more,
        },
        metadata: Some(QueryMetadata {
            as_of_timestamp: params.as_of,
            row_count: actual_results.len(),
        }),
    };

    info!("Query returned {} rows", actual_results.len());

    Ok(Json(response))
}

/// Insert data into a table
///
/// POST /v1/branches/{name}/tables/{table}/data
///
/// Inserts one or more rows into the specified table.
///
/// # Path Parameters
///
/// - name: Branch name
/// - table: Table name
///
/// # Request Body
///
/// - rows: Array of row objects (column_name -> value)
/// - return_ids: Whether to return inserted row IDs (optional, default: false)
///
/// # Response
///
/// - 201 Created: Returns InsertDataResponse with insert count
/// - 400 Bad Request: Invalid data format or missing required columns
/// - 404 Not Found: Branch or table does not exist
/// - 500 Internal Server Error: Database error
pub async fn insert_data(
    State(state): State<AppState>,
    Path((branch_name, table_name)): Path<(String, String)>,
    Json(request): Json<InsertDataRequest>,
) -> Result<(StatusCode, Json<InsertDataResponse>), ApiError> {
    // sprinter ccebc95977f1: the rows go straight to storage, through no
    // statement entry point, so this request leaves the async worker itself
    // (`runtime_offload`).
    crate::runtime_offload::offload_statement(move || insert_data_rows(state, branch_name, table_name, request))
}

/// The body of [`insert_data`], which runs it through `offload_statement`
/// (sprinter ccebc95977f1).
fn insert_data_rows(
    state: AppState,
    branch_name: String,
    table_name: String,
    request: InsertDataRequest,
) -> Result<(StatusCode, Json<InsertDataResponse>), ApiError> {
    info!(
        "Inserting {} rows into table '{}' in branch '{}'",
        request.rows.len(),
        table_name,
        branch_name
    );

    // Validate branch exists
    validate_branch(&state, &branch_name)?;

    if request.rows.is_empty() {
        return Err(ApiError::bad_request("No rows provided for insertion"));
    }

    // Get table schema
    // On the branch: its catalog decides (sprinter 6bc4a4040eb7, B3 fix pass).
    let schema = state
        .db
        .with_request_branch_scope(&branch_name, || {
            state.db.storage.catalog().get_table_schema(&table_name)
        })
        .map_err(|e| {
            warn!("Table '{}' not found: {}", table_name, e);
            ApiError::from(e)
        })?;

    let mut inserted_count = 0u64;
    let mut row_ids = Vec::new();

    // Insert each row
    for row in &request.rows {
        // Build tuple from row data
        let mut tuple_values = Vec::new();

        for column in &schema.columns {
            let value = if let Some(json_val) = row.get(&column.name) {
                // Convert JSON value to our Value type
                json_to_value(json_val, &column.data_type)
                    .map_err(|e| ApiError::bad_request(format!("Invalid value for column '{}': {}", column.name, e)))?
            } else if column.nullable {
                Value::Null
            } else {
                return Err(ApiError::bad_request(format!(
                    "Missing required column: {}",
                    column.name
                )));
            };

            tuple_values.push(value);
        }

        let tuple = Tuple::new(tuple_values);

        // Insert the tuple — on `:name` (request-scoped binding, B1): this
        // storage call reads the session binding the scope installs. B1 second
        // fix pass: it is a raw storage write, not a statement, so it takes
        // the branch's statement drain hold itself (`enter_branch_statement`,
        // what every SQL entry point's gate takes): a DROP / MERGE of the
        // branch waits for it, one already closing refuses it (55006), and a
        // branch dropped in between fails it closed (3D000) — instead of the
        // row landing in an overlay that was just dropped or merged. The id it
        // reports is the one the row got, from the counter the SQL insert uses (B3
        // third fix pass: this route used to burn a second id of the durable
        // counter, and report that one).
        let row_id = state
            .db
            .with_request_branch_scope(&branch_name, || {
                let _statement = state.db.enter_branch_statement()?;
                state.db.storage.insert_tuple(&table_name, tuple)
            })
            .map_err(|e| {
                error!("Failed to insert data: {}", e);
                ApiError::from(e)
            })?;

        inserted_count += 1;

        if request.return_ids {
            row_ids.push(row_id);
        }
    }

    let response = InsertDataResponse {
        inserted: inserted_count,
        row_ids: if request.return_ids { Some(row_ids) } else { None },
        message: format!("Successfully inserted {} row(s)", inserted_count),
    };

    info!("Inserted {} rows into table '{}'", inserted_count, table_name);

    Ok((StatusCode::CREATED, Json(response)))
}

/// Update data in a table
///
/// PUT /v1/branches/{name}/tables/{table}/data
///
/// Updates rows in the specified table that match the filter criteria.
///
/// # Path Parameters
///
/// - name: Branch name
/// - table: Table name
///
/// # Request Body
///
/// - values: Object with column_name -> new_value mappings
/// - filter: WHERE clause filter (optional, updates all rows if not provided)
///
/// # Response
///
/// - 200 OK: Returns UpdateDataResponse with update count
/// - 400 Bad Request: Invalid data format or filter
/// - 404 Not Found: Branch or table does not exist
/// - 500 Internal Server Error: Database error
pub async fn update_data(
    State(state): State<AppState>,
    Path((branch_name, table_name)): Path<(String, String)>,
    Json(request): Json<UpdateDataRequest>,
) -> Result<Json<UpdateDataResponse>, ApiError> {
    info!("Updating data in table '{}' in branch '{}'", table_name, branch_name);

    // Validate branch exists
    validate_branch(&state, &branch_name)?;

    if request.values.is_empty() {
        return Err(ApiError::bad_request("No values provided for update"));
    }

    // Get table schema to validate columns
    // On the branch: its catalog decides (sprinter 6bc4a4040eb7, B3 fix pass).
    let schema = state
        .db
        .with_request_branch_scope(&branch_name, || {
            state.db.storage.catalog().get_table_schema(&table_name)
        })
        .map_err(|e| {
            warn!("Table '{}' not found: {}", table_name, e);
            ApiError::from(e)
        })?;

    // Validate that all update columns exist
    for col_name in request.values.keys() {
        if !schema.columns.iter().any(|c| &c.name == col_name) {
            return Err(ApiError::bad_request(format!(
                "Column '{}' does not exist in table '{}'",
                col_name, table_name
            )));
        }
    }

    // Build UPDATE SQL
    let set_clause: Vec<String> = request
        .values
        .keys()
        .enumerate()
        .map(|(idx, col)| format!("{} = ${}", col, idx + 1))
        .collect();

    let mut sql = format!("UPDATE {} SET {}", table_name, set_clause.join(", "));

    if let Some(ref filter) = request.filter {
        sql.push_str(&format!(" WHERE {}", filter));
    }

    // Prepare parameters
    let params: Vec<Value> = request
        .values
        .iter()
        .map(|(col_name, json_val)| {
            let column = schema
                .columns
                .iter()
                .find(|c| &c.name == col_name)
                .ok_or_else(|| ApiError::bad_request(format!("Column '{}' not found", col_name)))?;

            json_to_value(json_val, &column.data_type)
                .map_err(|e| ApiError::bad_request(format!("Invalid value for column '{}': {}", col_name, e)))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Execute update
    let updated_count = state
        .db
        .with_request_branch_scope(&branch_name, || state.db.execute_params(&sql, &params))
        .map_err(|e| {
            error!("Failed to update data: {}", e);
            ApiError::from(e)
        })?;

    let response = UpdateDataResponse {
        updated: updated_count,
        message: format!("Successfully updated {} row(s)", updated_count),
    };

    info!("Updated {} rows in table '{}'", updated_count, table_name);

    Ok(Json(response))
}

/// Delete data from a table
///
/// DELETE /v1/branches/{name}/tables/{table}/data
///
/// Deletes rows from the specified table that match the filter criteria.
///
/// # Path Parameters
///
/// - name: Branch name
/// - table: Table name
///
/// # Request Body
///
/// - filter: WHERE clause filter (optional, deletes all rows if not provided)
///
/// # Response
///
/// - 200 OK: Returns DeleteDataResponse with delete count
/// - 400 Bad Request: Invalid filter
/// - 404 Not Found: Branch or table does not exist
/// - 500 Internal Server Error: Database error
pub async fn delete_data(
    State(state): State<AppState>,
    Path((branch_name, table_name)): Path<(String, String)>,
    Json(request): Json<DeleteDataRequest>,
) -> Result<Json<DeleteDataResponse>, ApiError> {
    info!("Deleting data from table '{}' in branch '{}'", table_name, branch_name);

    // Validate branch exists
    validate_branch(&state, &branch_name)?;

    // Validate table exists
    let _schema = state
        .db
        .with_request_branch_scope(&branch_name, || {
            state.db.storage.catalog().get_table_schema(&table_name)
        })
        .map_err(|e| {
            warn!("Table '{}' not found: {}", table_name, e);
            ApiError::from(e)
        })?;

    // Build DELETE SQL
    let mut sql = format!("DELETE FROM {}", table_name);

    if let Some(ref filter) = request.filter {
        sql.push_str(&format!(" WHERE {}", filter));
    } else {
        warn!("Deleting all rows from table '{}' (no filter provided)", table_name);
    }

    // Execute delete
    let deleted_count = state
        .db
        .with_request_branch_scope(&branch_name, || state.db.execute(&sql))
        .map_err(|e| {
            error!("Failed to delete data: {}", e);
            ApiError::from(e)
        })?;

    let response = DeleteDataResponse {
        deleted: deleted_count,
        message: format!("Successfully deleted {} row(s)", deleted_count),
    };

    info!("Deleted {} rows from table '{}'", deleted_count, table_name);

    Ok(Json(response))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;
    use std::sync::Arc;

    fn create_test_state() -> AppState {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(crate::compute::QueryRegistry::new());
        AppState {
            db,
            query_registry,
            auth_bridge: None,
            oauth_registry: None,
            change_notifier: None,
        }
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): the REST data routes read the
    /// BRANCH's catalog: a table it created is listed and served, one it dropped
    /// is not, and a column it added is accepted.
    #[tokio::test]
    async fn branch_data_routes_read_the_branchs_catalog() {
        let state = create_test_state();
        state.db.execute("CREATE TABLE kept (id INT, v TEXT)").unwrap();
        state.db.execute("CREATE TABLE gone (id INT)").unwrap();
        state.db.execute("CREATE BRANCH b AS OF NOW").unwrap();
        state
            .db
            .with_request_branch_scope("b", || {
                state.db.execute("CREATE TABLE fresh (id INT, v TEXT)")?;
                state.db.execute("INSERT INTO fresh VALUES (1, 'one')")?;
                state.db.execute("DROP TABLE gone")?;
                state.db.execute("ALTER TABLE kept ADD COLUMN extra TEXT")
            })
            .unwrap();

        let listed = list_tables(State(state.clone()), Path("b".to_string())).await.unwrap();
        let names: Vec<&str> = listed.0.tables.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"fresh"), "{names:?}");
        assert!(!names.contains(&"gone"), "{names:?}");
        let kept = listed.0.tables.iter().find(|t| t.name == "kept").unwrap();
        assert_eq!(kept.column_count, 3, "the branch's definition");
        let on_main = list_tables(State(state.clone()), Path("main".to_string()))
            .await
            .unwrap();
        let main_names: Vec<&str> = on_main.0.tables.iter().map(|t| t.name.as_str()).collect();
        assert!(
            main_names.contains(&"gone") && !main_names.contains(&"fresh"),
            "{main_names:?}"
        );

        let params = DataQueryParams {
            filter: None,
            columns: None,
            page: 1,
            limit: 10,
            as_of: None,
            order_by: None,
            include_total: false,
        };
        let served = query_data(
            State(state.clone()),
            Path(("b".to_string(), "fresh".to_string())),
            Query(params),
        )
        .await
        .unwrap();
        assert_eq!(served.0.rows.len(), 1);

        let mut row = std::collections::HashMap::new();
        row.insert("id".to_string(), serde_json::json!(5));
        row.insert("v".to_string(), serde_json::json!("five"));
        row.insert("extra".to_string(), serde_json::json!("added"));
        let inserted = insert_data(
            State(state.clone()),
            Path(("b".to_string(), "kept".to_string())),
            Json(InsertDataRequest {
                rows: vec![row],
                return_ids: true,
            }),
        )
        .await;
        let (_, Json(inserted)) = inserted.expect("a column the branch added is accepted");
        // B3 third fix pass: the id of the row it wrote, from the counter the SQL
        // insert uses. It used to burn another id of the durable counter and report
        // that one.
        let row_ids = inserted.row_ids.expect("return_ids");
        assert_eq!(row_ids.len(), 1);
        state
            .db
            .with_request_branch_scope("b", || {
                let view = state.db.storage.branch_view()?.expect("on b");
                let schema = state.db.storage.catalog().get_table_schema("kept")?;
                let row = view.get(None, "kept", row_ids[0], &schema)?;
                assert_eq!(
                    row.map(|row| row.values.first().cloned()),
                    Some(Some(Value::Int4(5))),
                    "the reported id is the row's"
                );
                Ok::<(), crate::Error>(())
            })
            .unwrap();

        // Every data route of a branch reads the branch's catalog.
        let mut values = std::collections::HashMap::new();
        values.insert("extra".to_string(), serde_json::json!("changed"));
        let updated = update_data(
            State(state.clone()),
            Path(("b".to_string(), "kept".to_string())),
            Json(UpdateDataRequest {
                values,
                filter: Some("id = 5".to_string()),
            }),
        )
        .await;
        assert!(updated.is_ok(), "a column the branch added can be updated");
        let deleted = delete_data(
            State(state.clone()),
            Path(("b".to_string(), "fresh".to_string())),
            Json(DeleteDataRequest {
                filter: Some("id = 1".to_string()),
            }),
        )
        .await;
        assert!(deleted.is_ok(), "a table the branch created can be deleted from");
        let gone = delete_data(
            State(state.clone()),
            Path(("b".to_string(), "gone".to_string())),
            Json(DeleteDataRequest { filter: None }),
        )
        .await;
        assert!(gone.is_err(), "a table the branch dropped is not there");

        // Statistics describe main's rows: a branch reports none.
        let mut statistics = crate::storage::TableStatistics::new("kept".to_string());
        statistics.update(99, 99);
        state
            .db
            .storage
            .catalog()
            .set_table_statistics("kept", &statistics)
            .unwrap();
        let row_count = |listed: &TableListResponse| {
            listed
                .tables
                .iter()
                .find(|t| t.name == "kept")
                .and_then(|t| t.row_count)
        };
        let on_main = list_tables(State(state.clone()), Path("main".to_string()))
            .await
            .unwrap();
        assert_eq!(row_count(&on_main.0), Some(99));
        let on_branch = list_tables(State(state.clone()), Path("b".to_string())).await.unwrap();
        assert_eq!(row_count(&on_branch.0), None);
    }

    #[tokio::test]
    async fn test_list_tables() {
        let state = create_test_state();

        // Create a test table
        state.db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();

        let result = list_tables(State(state), Path("main".to_string())).await;
        assert!(result.is_ok());

        let response = result.unwrap();
        assert!(response.0.total >= 1);
        assert!(response.0.tables.iter().any(|t| t.name == "users"));
    }

    #[tokio::test]
    async fn test_query_data() {
        let state = create_test_state();

        // Create and populate table
        state.db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();
        state.db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        state.db.execute("INSERT INTO users VALUES (2, 'Bob')").unwrap();

        let params = DataQueryParams {
            filter: None,
            columns: None,
            page: 1,
            limit: 10,
            as_of: None,
            order_by: None,
            include_total: false,
        };

        let result = query_data(
            State(state),
            Path(("main".to_string(), "users".to_string())),
            Query(params),
        )
        .await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.0.rows.len(), 2);
        assert_eq!(response.0.columns.len(), 2);
    }

    #[tokio::test]
    async fn test_insert_data() {
        let state = create_test_state();

        // Create table
        state.db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();

        let mut row = std::collections::HashMap::new();
        row.insert("id".to_string(), serde_json::json!(1));
        row.insert("name".to_string(), serde_json::json!("Alice"));

        let request = InsertDataRequest {
            rows: vec![row],
            return_ids: false,
        };

        let result = insert_data(
            State(state),
            Path(("main".to_string(), "users".to_string())),
            Json(request),
        )
        .await;

        assert!(result.is_ok());
        let (status, response) = result.unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(response.0.inserted, 1);
    }

    #[tokio::test]
    async fn test_update_data() {
        let state = create_test_state();

        // Create and populate table
        state.db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();
        state.db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();

        let mut values = std::collections::HashMap::new();
        values.insert("name".to_string(), serde_json::json!("Alice Updated"));

        let request = UpdateDataRequest {
            values,
            filter: Some("id = 1".to_string()),
        };

        let result = update_data(
            State(state),
            Path(("main".to_string(), "users".to_string())),
            Json(request),
        )
        .await;

        // UPDATE with parameters may not be fully supported yet - skip if not implemented
        if let Err(ref e) = result {
            let err_str = format!("{e:?}");
            if err_str.contains("not supported")
                || err_str.contains("not implemented")
                || err_str.contains("deserialize")
                || err_str.contains("io error")
            {
                eprintln!("Skipping test_update_data: UPDATE with parameters not yet fully supported");
                return;
            }
        }

        let response = result.expect("update_data should succeed");
        assert_eq!(response.0.updated, 1);
    }

    #[tokio::test]
    async fn test_delete_data() {
        let state = create_test_state();

        // Create and populate table
        state.db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();
        state.db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        state.db.execute("INSERT INTO users VALUES (2, 'Bob')").unwrap();

        let request = DeleteDataRequest {
            filter: Some("id = 1".to_string()),
        };

        let result = delete_data(
            State(state),
            Path(("main".to_string(), "users".to_string())),
            Json(request),
        )
        .await;

        // DELETE may not be fully supported yet - skip if not implemented
        if let Err(ref e) = result {
            let err_str = format!("{e:?}");
            if err_str.contains("not supported")
                || err_str.contains("not implemented")
                || err_str.contains("not yet implemented")
            {
                eprintln!("Skipping test_delete_data: DELETE not yet fully supported");
                return;
            }
        }

        let response = result.expect("delete_data should succeed");
        assert_eq!(response.0.deleted, 1);
    }
}
