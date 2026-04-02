//! Branch operation handlers
//!
//! Implements HTTP request handlers for branch CRUD operations.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use tracing::{info, warn, error};

use crate::api::{
    models::{
        ApiError,
        CreateBranchRequest,
        BranchResponse,
        BranchListResponse,
        MergeBranchRequest,
        MergeBranchResponse,
    },
    server::AppState,
};
use crate::storage::BranchOptions;

/// List all branches
///
/// GET /v1/branches
///
/// Returns a list of all active branches in the database.
///
/// # Response
///
/// - 200 OK: Returns BranchListResponse with all branches
/// - 500 Internal Server Error: Database error
/// - 501 Not Implemented: Branching not enabled
pub async fn list_branches(
    State(state): State<AppState>,
) -> Result<Json<BranchListResponse>, ApiError> {
    info!("Listing all branches");

    // Access the branch manager from storage
    let branch_manager = state.db.storage.branch_manager()
        .ok_or_else(|| ApiError::new(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "BranchingNotEnabled",
            "Database branching is not enabled for this instance"
        ))?;

    let branches = branch_manager.list_branches()
        .map_err(|e| {
            error!("Failed to list branches: {}", e);
            ApiError::from(e)
        })?;

    let response = BranchListResponse {
        total: branches.len(),
        branches: branches.into_iter().map(BranchResponse::from).collect(),
    };

    info!("Found {} branches", response.total);

    Ok(Json(response))
}

/// Create a new branch
///
/// POST /v1/branches
///
/// Creates a new database branch with the specified configuration.
///
/// # Request Body
///
/// - name: Branch name (required)
/// - parent: Parent branch name (optional, defaults to "main")
/// - snapshot_id: Snapshot to branch from (optional, defaults to current)
/// - options: Branch options (optional)
///
/// # Response
///
/// - 201 Created: Returns BranchResponse with created branch details
/// - 400 Bad Request: Invalid request
/// - 409 Conflict: Branch already exists
/// - 500 Internal Server Error: Database error
/// - 501 Not Implemented: Branching not enabled
pub async fn create_branch(
    State(state): State<AppState>,
    Json(request): Json<CreateBranchRequest>,
) -> Result<(StatusCode, Json<BranchResponse>), ApiError> {
    info!("Creating branch: {}", request.name);

    // Validate branch name
    if request.name.is_empty() {
        warn!("Invalid branch name: empty");
        return Err(ApiError::bad_request("Branch name cannot be empty"));
    }

    // Access the branch manager from storage
    let branch_manager = state.db.storage.branch_manager()
        .ok_or_else(|| ApiError::new(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "BranchingNotEnabled",
            "Database branching is not enabled for this instance"
        ))?;

    // Get current snapshot ID or use provided one
    let snapshot_id = request.snapshot_id.unwrap_or_else(|| {
        branch_manager.current_timestamp()
    });

    // Convert options
    let options = request.options
        .map(BranchOptions::from)
        .unwrap_or_default();

    // Create the branch
    let branch_id = branch_manager.create_branch(
        &request.name,
        request.parent.as_deref(),
        snapshot_id,
        options,
    ).map_err(|e| {
        error!("Failed to create branch '{}': {}", request.name, e);
        ApiError::from(e)
    })?;

    info!("Branch '{}' created with ID: {}", request.name, branch_id);

    // Get the created branch metadata
    let metadata = branch_manager.get_branch_by_name(&request.name)
        .map_err(|e| {
            error!("Failed to retrieve created branch: {}", e);
            ApiError::from(e)
        })?;

    Ok((StatusCode::CREATED, Json(BranchResponse::from(metadata))))
}

/// Get branch details
///
/// GET /v1/branches/:name
///
/// Retrieves detailed information about a specific branch.
///
/// # Path Parameters
///
/// - name: Branch name
///
/// # Response
///
/// - 200 OK: Returns BranchResponse with branch details
/// - 404 Not Found: Branch does not exist
/// - 500 Internal Server Error: Database error
/// - 501 Not Implemented: Branching not enabled
pub async fn get_branch(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<BranchResponse>, ApiError> {
    info!("Getting branch: {}", name);

    // Access the branch manager from storage
    let branch_manager = state.db.storage.branch_manager()
        .ok_or_else(|| ApiError::new(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "BranchingNotEnabled",
            "Database branching is not enabled for this instance"
        ))?;

    let metadata = branch_manager.get_branch_by_name(&name)
        .map_err(|e| {
            warn!("Branch '{}' not found: {}", name, e);
            ApiError::from(e)
        })?;

    info!("Found branch '{}'", name);

    Ok(Json(BranchResponse::from(metadata)))
}

/// Delete a branch
///
/// DELETE /v1/branches/:name
///
/// Deletes a branch. The branch must not have any child branches.
/// Cannot delete the main branch.
///
/// # Path Parameters
///
/// - name: Branch name
///
/// # Query Parameters
///
/// - if_exists: If true, don't error if branch doesn't exist (optional)
///
/// # Response
///
/// - 204 No Content: Branch deleted successfully
/// - 400 Bad Request: Cannot delete main branch or branch has children
/// - 404 Not Found: Branch does not exist (unless if_exists=true)
/// - 500 Internal Server Error: Database error
/// - 501 Not Implemented: Branching not enabled
pub async fn delete_branch(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    info!("Deleting branch: {}", name);

    // Access the branch manager from storage
    let branch_manager = state.db.storage.branch_manager()
        .ok_or_else(|| ApiError::new(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "BranchingNotEnabled",
            "Database branching is not enabled for this instance"
        ))?;

    // Drop the branch (if_exists = false for strict error handling)
    branch_manager.drop_branch(&name, false)
        .map_err(|e| {
            error!("Failed to delete branch '{}': {}", name, e);
            ApiError::from(e)
        })?;

    info!("Branch '{}' deleted successfully", name);

    Ok(StatusCode::NO_CONTENT)
}

/// Merge branches
///
/// POST /v1/branches/:name/merge
///
/// Merges the source branch (specified in path) into the target branch
/// (specified in request body).
///
/// # Path Parameters
///
/// - name: Source branch name to merge from
///
/// # Request Body
///
/// - target: Target branch name to merge into
/// - strategy: Merge strategy (auto, manual, theirs, ours)
///
/// # Response
///
/// - 200 OK: Merge completed, returns MergeBranchResponse
/// - 409 Conflict: Merge conflicts detected (if strategy is manual)
/// - 422 Unprocessable Entity: Branches are not in valid state for merge
/// - 404 Not Found: Source or target branch not found
/// - 500 Internal Server Error: Database error
/// - 501 Not Implemented: Branching not enabled
pub async fn merge_branch(
    State(state): State<AppState>,
    Path(source_name): Path<String>,
    Json(request): Json<MergeBranchRequest>,
) -> Result<Json<MergeBranchResponse>, ApiError> {
    info!("Merging branch '{}' into '{}'", source_name, request.target);

    // Access the branch manager from storage
    let branch_manager = state.db.storage.branch_manager()
        .ok_or_else(|| ApiError::new(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "BranchingNotEnabled",
            "Database branching is not enabled for this instance"
        ))?;

    // Perform the merge
    let merge_result = branch_manager.merge_branch(
        &source_name,
        &request.target,
        request.strategy.into(),
    ).map_err(|e| {
        error!("Failed to merge '{}' into '{}': {}", source_name, request.target, e);
        ApiError::from(e)
    })?;

    let message = if merge_result.completed {
        if merge_result.conflicts.is_empty() {
            format!(
                "Successfully merged {} keys from '{}' into '{}' with no conflicts",
                merge_result.merged_keys,
                source_name,
                request.target
            )
        } else {
            format!(
                "Successfully merged {} keys from '{}' into '{}' with {} conflicts resolved",
                merge_result.merged_keys,
                source_name,
                request.target,
                merge_result.conflicts.len()
            )
        }
    } else {
        format!(
            "Merge from '{}' into '{}' failed due to {} conflicts (manual strategy requires conflict resolution)",
            source_name,
            request.target,
            merge_result.conflicts.len()
        )
    };

    info!("{}", message);

    Ok(Json(MergeBranchResponse {
        merge_timestamp: merge_result.merge_timestamp,
        merged_keys: merge_result.merged_keys,
        conflicts: merge_result.conflicts.into_iter()
            .map(|c| c.into())
            .collect(),
        completed: merge_result.completed,
        message,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::EmbeddedDatabase;
    use crate::api::models::branch::MergeStrategyDto;
    use std::sync::Arc;

    fn create_test_state() -> AppState {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let query_registry = Arc::new(crate::compute::QueryRegistry::new());
        AppState { db, query_registry, auth_bridge: None, oauth_registry: None, change_notifier: None }
    }

    #[tokio::test]
    async fn test_list_branches() {
        let state = create_test_state();

        let result = list_branches(State(state)).await;
        assert!(result.is_ok());

        let response = result.unwrap();
        // Should have at least the main branch
        assert!(response.0.total >= 1);
    }

    #[tokio::test]
    async fn test_create_branch() {
        let state = create_test_state();

        let request = CreateBranchRequest {
            name: "test-branch".to_string(),
            parent: Some("main".to_string()),
            snapshot_id: None,
            options: None,
        };

        let result = create_branch(State(state), Json(request)).await;
        assert!(result.is_ok());

        let (status, response) = result.unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(response.0.name, "test-branch");
    }

    #[tokio::test]
    async fn test_create_branch_empty_name() {
        let state = create_test_state();

        let request = CreateBranchRequest {
            name: "".to_string(),
            parent: None,
            snapshot_id: None,
            options: None,
        };

        let result = create_branch(State(state), Json(request)).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_get_branch() {
        let state = create_test_state();

        // Get the main branch
        let result = get_branch(State(state), Path("main".to_string())).await;
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.0.name, "main");
    }

    #[tokio::test]
    async fn test_get_branch_not_found() {
        let state = create_test_state();

        let result = get_branch(State(state), Path("nonexistent".to_string())).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_delete_branch() {
        let state = create_test_state();

        // Create a branch first
        let request = CreateBranchRequest {
            name: "temp-branch".to_string(),
            parent: Some("main".to_string()),
            snapshot_id: None,
            options: None,
        };
        create_branch(State(state.clone()), Json(request)).await.unwrap();

        // Delete it
        let result = delete_branch(State(state), Path("temp-branch".to_string())).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_delete_main_branch() {
        let state = create_test_state();

        let result = delete_branch(State(state), Path("main".to_string())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_merge_branch() {
        let state = create_test_state();

        // Create a branch to merge
        let create_req = CreateBranchRequest {
            name: "feature".to_string(),
            parent: Some("main".to_string()),
            snapshot_id: None,
            options: None,
        };
        create_branch(State(state.clone()), Json(create_req)).await.unwrap();

        // Merge it back
        let merge_req = MergeBranchRequest {
            target: "main".to_string(),
            strategy: MergeStrategyDto::Auto,
        };

        let result = merge_branch(State(state), Path("feature".to_string()), Json(merge_req)).await;
        assert!(result.is_ok());

        let response = result.unwrap();
        assert!(response.0.completed);
    }
}
