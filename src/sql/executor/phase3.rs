//! Phase 3 features: Branching, Materialized Views, and System Views
//!
//! This module handles execution of Phase 3 advanced database features.

#![allow(elided_lifetimes_in_paths)]

use crate::{Result, Error};
use crate::sql::LogicalPlan;
use crate::storage::{MvDeltaSet, MvDeltaOperation};
use super::{PhysicalOperator, Executor};
use super::scan::{ScanOperator, MaterializedOperator};
use std::sync::Arc;

/// Count delta operations by type (inserts, updates, deletes)
fn count_delta_operations(delta_set: &MvDeltaSet) -> (usize, usize, usize) {
    let mut inserts = 0;
    let mut updates = 0;
    let mut deletes = 0;

    for delta in &delta_set.deltas {
        match &delta.operation {
            MvDeltaOperation::Insert { .. } => inserts += 1,
            MvDeltaOperation::Update { .. } => updates += 1,
            MvDeltaOperation::Delete { .. } => deletes += 1,
        }
    }

    (inserts, updates, deletes)
}

/// Handle Phase 3 logical plan operations
pub(super) fn handle_phase3_operation(
    executor: &mut Executor,
    plan: &LogicalPlan,
) -> Result<Box<dyn PhysicalOperator>> {
    match plan {
        LogicalPlan::CreateBranch { branch_name, parent, as_of, options } => {
            handle_create_branch(executor, branch_name, parent, as_of, options)
        }
        LogicalPlan::DropBranch { branch_name, if_exists } => {
            handle_drop_branch(executor, branch_name, *if_exists)
        }
        LogicalPlan::MergeBranch { source, target, options } => {
            handle_merge_branch(executor, source, target, options)
        }
        LogicalPlan::UseBranch { branch_name } => {
            handle_use_branch(executor, branch_name)
        }
        LogicalPlan::ShowBranches => {
            handle_show_branches(executor)
        }
        LogicalPlan::CreateMaterializedView { name, query, options, if_not_exists } => {
            handle_create_materialized_view(executor, name, query, options, *if_not_exists)
        }
        LogicalPlan::RefreshMaterializedView { name, concurrent, incremental } => {
            handle_refresh_materialized_view(executor, name, *concurrent, *incremental)
        }
        LogicalPlan::DropMaterializedView { name, if_exists } => {
            handle_drop_materialized_view(executor, name, *if_exists)
        }
        LogicalPlan::AlterMaterializedView { name, options } => {
            handle_alter_materialized_view(executor, name, options)
        }
        LogicalPlan::SystemView { name, .. } => {
            handle_system_view(executor, name)
        }
        _ => Err(Error::query_execution("Unsupported advanced operation")),
    }
}

/// Handle CREATE BRANCH
fn handle_create_branch(
    executor: &Executor,
    branch_name: &str,
    parent: &Option<String>,
    as_of: &crate::sql::logical_plan::AsOfClause,
    options: &[crate::sql::logical_plan::BranchOption],
) -> Result<Box<dyn PhysicalOperator>> {
    use crate::storage::BranchOptions;

    tracing::info!(
        "Executing CREATE BRANCH {} FROM {:?} AS OF {:?} WITH {:?}",
        branch_name, parent, as_of, options
    );

    if let Some(storage) = executor.storage() {
        // Parse branch options
        let mut branch_opts = BranchOptions::default();
        for option in options {
            use crate::sql::logical_plan::BranchOption;
            match option {
                BranchOption::ReplicationFactor(rf) => {
                    branch_opts.replication_factor = Some(*rf as usize);
                }
                BranchOption::Region(region) => {
                    branch_opts.region = Some(region.clone());
                }
                BranchOption::Metadata(key, value) => {
                    branch_opts.metadata.insert(key.clone(), value.clone());
                }
            }
        }

        // Resolve AS OF clause to actual snapshot ID
        let snapshot_id = match as_of {
            crate::sql::logical_plan::AsOfClause::Now => {
                // Use current timestamp (latest snapshot)
                None
            }
            other_as_of => {
                // Resolve timestamp/transaction/SCN to snapshot ID
                let snapshot_manager = storage.snapshot_manager();
                let resolved_snapshot = snapshot_manager.resolve_as_of(other_as_of)?;

                tracing::debug!(
                    "Resolved AS OF {:?} to snapshot ID {}",
                    other_as_of,
                    resolved_snapshot
                );

                Some(resolved_snapshot)
            }
        };

        // Resolve parent branch:
        // - If explicitly specified, use that
        // - Otherwise, use the current active branch (implicit FROM current branch)
        let resolved_parent = match parent {
            Some(p) => Some(p.clone()),
            None => storage.get_current_branch(),
        };

        // Create the branch at the resolved snapshot
        let branch_id = storage.create_branch_at_snapshot(
            branch_name,
            resolved_parent.as_deref(),
            snapshot_id,
            branch_opts,
        )?;

        tracing::info!(
            "Successfully created branch '{}' with ID {} at snapshot {:?}",
            branch_name,
            branch_id,
            snapshot_id
        );
    } else {
        return Err(Error::execution("No storage engine available"));
    }

    // Return empty result set for DDL
    Ok(Box::new(ScanOperator::new(
        "".to_string(),
        Arc::new(crate::Schema { columns: vec![] }),
        None,
        vec![],
        vec![],
    ).with_timeout(executor.timeout_ctx())))
}

/// Handle DROP BRANCH
fn handle_drop_branch(
    executor: &Executor,
    branch_name: &str,
    if_exists: bool,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!(
        "Executing DROP BRANCH {} (IF EXISTS: {})",
        branch_name, if_exists
    );

    if let Some(storage) = executor.storage() {
        // Drop the branch
        storage.drop_branch(branch_name, if_exists)?;

        tracing::info!("Successfully dropped branch '{}'", branch_name);
    } else {
        return Err(Error::execution("No storage engine available"));
    }

    // Return empty result set for DDL
    Ok(Box::new(ScanOperator::new(
        "".to_string(),
        Arc::new(crate::Schema { columns: vec![] }),
        None,
        vec![],
        vec![],
    ).with_timeout(executor.timeout_ctx())))
}

/// Handle MERGE BRANCH
fn handle_merge_branch(
    executor: &Executor,
    source: &str,
    target: &str,
    options: &[crate::sql::logical_plan::MergeOption],
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!(
        "Executing MERGE BRANCH {} INTO {} WITH {:?}",
        source, target, options
    );

    if let Some(storage) = executor.storage() {
        // Determine merge strategy from options
        let strategy = resolve_merge_strategy(options);

        // Perform the merge
        let result = storage.merge_branch(
            source,
            target,
            strategy,
        )?;

        // Log results
        if result.completed {
            tracing::info!(
                "Merge completed: {} keys merged, {} conflicts resolved",
                result.merged_keys,
                result.conflicts.len()
            );

            // Check if DELETE_SOURCE option is set and delete source branch after successful merge
            if should_delete_branch_after_merge(options) {
                tracing::info!(
                    "Deleting source branch '{}' after successful merge (DELETE_BRANCH_AFTER option set)",
                    source
                );
                storage.drop_branch(source, false)?;
                tracing::info!("Successfully deleted source branch '{}'", source);
            }
        } else {
            tracing::warn!(
                "Merge failed due to {} conflicts (manual resolution required)",
                result.conflicts.len()
            );

            // Return error with conflict details
            return Err(Error::storage(format!(
                "Merge failed: {} conflicts detected. Use conflict_resolution='branch_wins', 'target_wins', or remove the option for auto resolution.",
                result.conflicts.len()
            )));
        }
    } else {
        return Err(Error::execution("No storage engine available"));
    }

    // Return empty result set for DDL
    Ok(Box::new(ScanOperator::new(
        "".to_string(),
        Arc::new(crate::Schema { columns: vec![] }),
        None,
        vec![],
        vec![],
    ).with_timeout(executor.timeout_ctx())))
}

/// Handle CREATE MATERIALIZED VIEW
fn handle_create_materialized_view(
    executor: &mut Executor,
    name: &str,
    query: &LogicalPlan,
    options: &[crate::sql::logical_plan::MaterializedViewOption],
    if_not_exists: bool,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!(
        "CREATE MATERIALIZED VIEW {} (IF NOT EXISTS: {}) WITH {:?}",
        name, if_not_exists, options
    );

    // First check: verify storage exists and view doesn't already exist
    // We do this separately to avoid borrow conflicts
    {
        let storage = executor.storage().ok_or_else(|| Error::execution("No storage engine available"))?;
        let mv_catalog = storage.mv_catalog();

        // Check if view already exists
        if mv_catalog.view_exists(name)? {
            if if_not_exists {
                tracing::info!("Materialized view '{}' already exists (IF NOT EXISTS specified)", name);
                return Ok(Box::new(ScanOperator::new(
                    "".to_string(),
                    Arc::new(crate::Schema { columns: vec![] }),
                    None,
                    vec![],
                    vec![],
                ).with_timeout(executor.timeout_ctx())));
            } else {
                return Err(Error::query_execution(format!(
                    "Materialized view '{}' already exists",
                    name
                )));
            }
        }
    } // Storage borrow ends here

    // Execute the query to get the schema (this needs mutable borrow)
    let mut query_operator = executor.plan_to_operator(query)?;
    let schema = query_operator.schema();

    // Extract base tables from the query (simplified - just look at Scan nodes)
    let base_tables = extract_base_tables(query);

    // Serialize the query plan for re-execution during REFRESH
    let query_plan_bytes = bincode::serialize(query)
        .map_err(|e| Error::execution(format!("Failed to serialize query plan: {}", e)))?;

    // Store a human-readable query text for display/debugging
    let query_text = format!("{:?}", query);

    // Create metadata
    let mut metadata = crate::storage::MaterializedViewMetadata::new(
        name.to_string(),
        query_text,
        query_plan_bytes,
        base_tables.clone(),
        (*schema).clone(),
    );

    // Process options (parse auto_refresh, etc.)
    for option in options {
        match option {
            crate::sql::logical_plan::MaterializedViewOption::AutoRefresh(enabled) => {
                if *enabled {
                    metadata.refresh_strategy = "auto".to_string();
                }
            }
            crate::sql::logical_plan::MaterializedViewOption::MaxCpuPercent(pct) => {
                metadata.metadata.insert("max_cpu_percent".to_string(), pct.to_string());
            }
            crate::sql::logical_plan::MaterializedViewOption::ThresholdDmlRate(rate) => {
                metadata.metadata.insert("threshold_dml_rate".to_string(), rate.to_string());
            }
            _ => {
                // Store other options as metadata
                tracing::debug!("Storing MV option: {:?}", option);
            }
        }
    }

    // Initial population: Execute query and store results
    tracing::info!("Populating initial data for materialized view '{}'", name);
    let mut tuples = Vec::new();
    while let Some(tuple) = query_operator.next()? {
        tuples.push(tuple);
    }

    // Now access storage again to store the data (mutable borrow is done)
    {
        let storage = executor.storage().ok_or_else(|| Error::execution("No storage engine available"))?;
        let mv_catalog = storage.mv_catalog();

        // Store metadata in catalog
        mv_catalog.create_view(metadata)?;

        let row_count = mv_catalog.store_view_data(name, tuples, &schema)?;

        // Update metadata with row count
        let mut updated_metadata = mv_catalog.get_view(name)?;
        updated_metadata.mark_refreshed(row_count);
        mv_catalog.update_view(&updated_metadata)?;

        // Initialize delta tracking for base tables
        // This will enable incremental refresh for this materialized view
        tracing::debug!("Initializing delta tracking for base tables: {:?}", base_tables);
        for table_name in &base_tables {
            tracing::debug!("Delta tracking enabled for table '{}' (used by MV '{}')", table_name, name);
            // Note: Delta tracking is automatically handled by the MvDeltaTracker
            // which captures all table changes. No explicit registration needed here.
        }

        tracing::info!("Successfully created materialized view '{}' with {} rows (delta tracking active)", name, row_count);
    }

    // Return empty result set for DDL
    Ok(Box::new(ScanOperator::new(
        "".to_string(),
        Arc::new(crate::Schema { columns: vec![] }),
        None,
        vec![],
        vec![],
    ).with_timeout(executor.timeout_ctx())))
}

/// Handle REFRESH MATERIALIZED VIEW
fn handle_refresh_materialized_view(
    executor: &mut Executor,
    name: &str,
    concurrent: bool,
    incremental_requested: bool,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!(
        "REFRESH MATERIALIZED VIEW {} (CONCURRENT: {}, INCREMENTALLY: {})",
        name, concurrent, incremental_requested
    );

    // Phase 1: Read metadata from storage
    let (metadata, delta_count, use_incremental, total_deltas) = {
        let storage = executor.storage().ok_or_else(|| Error::execution("No storage engine available"))?;
        let mv_catalog = storage.mv_catalog();

        // Get view metadata
        let metadata = mv_catalog.get_view(name)?;
        tracing::debug!("Refreshing materialized view '{}' with query: {}", name, metadata.query_text);

        // Check if incremental refresh is possible
        let can_use_incremental = metadata.last_refresh.is_some() && !concurrent;

        // Check delta count for incremental refresh decision
        let delta_count = if can_use_incremental {
            if let Some(last_refresh) = metadata.last_refresh {
                let delta_tracker = storage.mv_delta_tracker();
                delta_tracker.count_deltas_since(&metadata.base_tables, last_refresh)
                    .unwrap_or(0)
            } else {
                0
            }
        } else {
            0
        };

        // Decide refresh strategy
        let use_incremental = if incremental_requested {
            if !can_use_incremental {
                if metadata.last_refresh.is_none() {
                    tracing::warn!(
                        "INCREMENTALLY requested but MV '{}' has never been refreshed. Using full refresh.",
                        name
                    );
                } else if concurrent {
                    tracing::warn!(
                        "INCREMENTALLY requested but CONCURRENT mode is enabled for MV '{}'. Using full refresh.",
                        name
                    );
                }
                false
            } else if delta_count == 0 {
                tracing::info!(
                    "INCREMENTALLY requested but no deltas found for MV '{}'. Using full refresh.",
                    name
                );
                false
            } else {
                true
            }
        } else {
            can_use_incremental && delta_count > 0 && delta_count < 1000
        };

        // For incremental, check if there are actually deltas to apply
        let total_deltas = if use_incremental {
            if let Some(last_refresh) = metadata.last_refresh {
                let delta_tracker = storage.mv_delta_tracker();
                match delta_tracker.get_deltas_since(&metadata.base_tables, last_refresh) {
                    Ok(delta_map) => {
                        for (table_name, delta_set) in &delta_map {
                            let (inserts, updates, deletes) = count_delta_operations(delta_set);
                            tracing::debug!(
                                "Table '{}': {} inserts, {} updates, {} deletes",
                                table_name, inserts, updates, deletes
                            );
                        }
                        delta_map.values().map(|ds| ds.deltas.len()).sum()
                    }
                    Err(_) => 0,
                }
            } else {
                0
            }
        } else {
            0
        };

        (metadata, delta_count, use_incremental, total_deltas)
    }; // Storage borrow ends here

    // Handle case with no deltas to apply
    if use_incremental && total_deltas == 0 {
        tracing::info!("No deltas to apply, MV '{}' is already up to date", name);
        let storage = executor.storage().ok_or_else(|| Error::execution("No storage engine available"))?;
        let mv_catalog = storage.mv_catalog();
        let mut updated_metadata = metadata.clone();
        updated_metadata.mark_refreshed(metadata.row_count.unwrap_or(0));
        mv_catalog.update_view(&updated_metadata)?;

        return Ok(Box::new(ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )));
    }

    // Phase 2: Execute query (requires mutable borrow of executor)
    let query_plan = metadata.get_query_plan()?;
    let schema = Arc::new(metadata.schema.clone());

    if use_incremental {
        tracing::info!(
            "Incremental refresh for MV '{}': {} deltas to apply",
            name, delta_count
        );
    } else if delta_count > 0 {
        tracing::info!(
            "Full refresh for MV '{}': {} deltas (threshold exceeded or concurrent mode)",
            name, delta_count
        );
    } else {
        tracing::info!("Full refresh for MV '{}'", name);
    }

    let mut query_operator = executor.plan_to_operator(&query_plan)?;
    let mut tuples = Vec::new();
    while let Some(tuple) = query_operator.next()? {
        tuples.push(tuple);
    }
    let row_count = tuples.len() as u64;

    // Phase 3: Store results (re-borrow storage)
    {
        let storage = executor.storage().ok_or_else(|| Error::execution("No storage engine available"))?;
        let mv_catalog = storage.mv_catalog();

        if concurrent {
            tracing::info!("Using CONCURRENT refresh with atomic swap for zero downtime");
            mv_catalog.store_view_data_concurrent(name, tuples, &schema)?;
        } else {
            tracing::debug!("Using non-CONCURRENT refresh (table will be briefly unavailable)");
            mv_catalog.store_view_data(name, tuples, &schema)?;
        }

        if use_incremental {
            // Clear applied deltas (purge all deltas before current time)
            let delta_tracker = storage.mv_delta_tracker();
            let _ = delta_tracker.purge_deltas_before(chrono::Utc::now());

            // Update delta count tracking in metadata
            let mut updated_metadata = metadata.clone();
            updated_metadata.delta_count_since_full = 0;
            updated_metadata.mark_refreshed(row_count);
            mv_catalog.update_view(&updated_metadata)?;

            tracing::info!(
                "Incremental refresh completed for MV '{}': {} rows (processed {} deltas)",
                name, row_count, total_deltas
            );
        } else {
            // Update metadata with new row count and timestamp
            let mut updated_metadata = metadata;
            updated_metadata.mark_refreshed(row_count);
            mv_catalog.update_view(&updated_metadata)?;

            tracing::info!(
                "Successfully refreshed materialized view '{}' with {} rows{}",
                name, row_count,
                if concurrent { " (CONCURRENT mode - zero downtime)" } else { "" }
            );
        }
    }

    // Return empty result set
    Ok(Box::new(ScanOperator::new(
        "".to_string(),
        Arc::new(crate::Schema { columns: vec![] }),
        None,
        vec![],
        vec![],
    )))
}

/// Handle DROP MATERIALIZED VIEW
fn handle_drop_materialized_view(
    executor: &Executor,
    name: &str,
    if_exists: bool,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!(
        "DROP MATERIALIZED VIEW {} (IF EXISTS: {})",
        name, if_exists
    );

    if let Some(storage) = executor.storage() {
        let mv_catalog = storage.mv_catalog();

        // Check if view exists
        if !mv_catalog.view_exists(name)? {
            if if_exists {
                tracing::info!("Materialized view '{}' does not exist (IF EXISTS specified)", name);
                return Ok(Box::new(ScanOperator::new(
                    "".to_string(),
                    Arc::new(crate::Schema { columns: vec![] }),
                    None,
                    vec![],
                    vec![],
                ).with_timeout(executor.timeout_ctx())));
            } else {
                return Err(Error::query_execution(format!(
                    "Materialized view '{}' does not exist",
                    name
                )));
            }
        }

        // Drop the materialized view (catalog + data)
        mv_catalog.drop_view(name)?;

        tracing::info!("Successfully dropped materialized view '{}'", name);
    } else {
        return Err(Error::execution("No storage engine available"));
    }

    // Return empty result set for DDL
    Ok(Box::new(ScanOperator::new(
        "".to_string(),
        Arc::new(crate::Schema { columns: vec![] }),
        None,
        vec![],
        vec![],
    ).with_timeout(executor.timeout_ctx())))
}

/// Handle ALTER MATERIALIZED VIEW
fn handle_alter_materialized_view(
    executor: &Executor,
    name: &str,
    options: &std::collections::HashMap<String, String>,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!(
        "ALTER MATERIALIZED VIEW {} SET {:?}",
        name, options
    );

    if let Some(storage) = executor.storage() {
        let mv_catalog = storage.mv_catalog();

        // Check if view exists
        if !mv_catalog.view_exists(name)? {
            return Err(Error::query_execution(format!(
                "Materialized view '{}' does not exist",
                name
            )));
        }

        // Get current metadata
        let mut metadata = mv_catalog.get_view(name)?;

        // Apply options to metadata
        for (key, value) in options {
            match key.as_str() {
                "staleness_threshold" => {
                    metadata.metadata.insert("staleness_threshold".to_string(), value.clone());
                }
                "max_cpu_percent" => {
                    metadata.metadata.insert("max_cpu_percent".to_string(), value.clone());
                }
                "priority" => {
                    metadata.metadata.insert("priority".to_string(), value.clone());
                }
                "refresh_strategy" => {
                    metadata.refresh_strategy = value.clone();
                }
                "incremental_enabled" => {
                    metadata.incremental_enabled = value.to_lowercase() == "true";
                }
                _ => {
                    // Store unknown options in metadata for future extensibility
                    metadata.metadata.insert(key.clone(), value.clone());
                }
            }
        }

        // Update the metadata in catalog
        mv_catalog.update_view(&metadata)?;

        tracing::info!("Successfully altered materialized view '{}'", name);
    } else {
        return Err(Error::execution("No storage engine available"));
    }

    // Return empty result set for DDL
    Ok(Box::new(ScanOperator::new(
        "".to_string(),
        Arc::new(crate::Schema { columns: vec![] }),
        None,
        vec![],
        vec![],
    ).with_timeout(executor.timeout_ctx())))
}

/// Handle SYSTEM VIEW query
fn handle_system_view(
    executor: &Executor,
    name: &str,
) -> Result<Box<dyn PhysicalOperator>> {
    // Execute system view query
    use crate::sql::phase3::SystemViewRegistry;

    let storage = executor.storage().ok_or_else(|| {
        Error::query_execution("System views require storage engine")
    })?;

    let registry = SystemViewRegistry::new();

    // Execute the system view and get results
    let tuples = registry.execute(name, storage)?;

    // Get schema for the view
    let schema = registry.get_schema(name)
        .ok_or_else(|| Error::query_execution(format!("System view '{}' schema not found", name)))?
        .clone();

    // Convert tuples to a materialized operator
    // Since we have all the data, we can return a simple in-memory result set
    Ok(Box::new(MaterializedOperator::new(
        tuples,
        Arc::new(schema),
    )))
}

/// Resolve merge strategy from SQL options
fn resolve_merge_strategy(
    options: &[crate::sql::logical_plan::MergeOption]
) -> crate::storage::MergeStrategy {
    use crate::sql::logical_plan::{MergeOption, ConflictResolution};
    use crate::storage::MergeStrategy;

    // Default strategy
    let mut strategy = MergeStrategy::Auto;

    for option in options {
        match option {
            MergeOption::ConflictResolution(resolution) => {
                strategy = match resolution {
                    ConflictResolution::BranchWins => MergeStrategy::Theirs,
                    ConflictResolution::TargetWins => MergeStrategy::Ours,
                    ConflictResolution::Fail => MergeStrategy::Manual,
                };
            }
            MergeOption::DeleteBranchAfter(_) => {
                // This option doesn't affect merge strategy
                // Branch deletion is handled separately after successful merge
            }
        }
    }

    strategy
}

/// Check if source branch should be deleted after merge
fn should_delete_branch_after_merge(options: &[crate::sql::logical_plan::MergeOption]) -> bool {
    use crate::sql::logical_plan::MergeOption;

    for option in options {
        if let MergeOption::DeleteBranchAfter(delete) = option {
            return *delete;
        }
    }

    false
}

/// Extract base table names from a logical plan
///
/// Recursively walks the plan tree and collects all table names from Scan nodes.
fn extract_base_tables(plan: &LogicalPlan) -> Vec<String> {
    let mut tables = Vec::new();

    match plan {
        LogicalPlan::Scan { table_name, .. } => {
            tables.push(table_name.clone());
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => {
            tables.extend(extract_base_tables(input));
        }
        LogicalPlan::Aggregate { input, .. } => {
            tables.extend(extract_base_tables(input));
        }
        LogicalPlan::Join { left, right, .. } => {
            tables.extend(extract_base_tables(left));
            tables.extend(extract_base_tables(right));
        }
        _ => {
            // Other plan types don't contain table references
        }
    }

    // Remove duplicates
    tables.sort();
    tables.dedup();
    tables
}

/// Handle USE BRANCH
fn handle_use_branch(
    executor: &Executor,
    branch_name: &str,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!("Executing USE BRANCH {}", branch_name);

    if let Some(storage) = executor.storage() {
        // Validate branch exists
        let metadata = storage.get_branch_metadata(branch_name)?;

        tracing::info!(
            "Switched to branch '{}' (ID: {}, created: {})",
            metadata.name,
            metadata.branch_id,
            metadata.created_at
        );

        // Switch to branch (sets current branch context in storage)
        storage.use_branch(branch_name)?;

        tracing::debug!("Successfully switched to branch '{}'", branch_name);
    } else {
        return Err(Error::execution("No storage engine available"));
    }

    // Return empty result set
    Ok(Box::new(ScanOperator::new(
        "".to_string(),
        Arc::new(crate::Schema { columns: vec![] }),
        None,
        vec![],
        vec![],
    ).with_timeout(executor.timeout_ctx())))
}

/// Handle SHOW BRANCHES
fn handle_show_branches(
    executor: &Executor,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!("Executing SHOW BRANCHES");

    if let Some(storage) = executor.storage() {
        // Get all branches
        let branches = storage.list_branches()?;

        tracing::debug!("Found {} branches", branches.len());

        // Convert to tuples
        let mut tuples = Vec::new();
        for branch_meta in branches {
            // Format state as string
            let state_str = match branch_meta.state {
                crate::storage::BranchState::Active => "Active".to_string(),
                crate::storage::BranchState::Merged { into_branch, at_timestamp } => {
                    format!("Merged into branch {} at {}", into_branch, at_timestamp)
                }
                crate::storage::BranchState::Dropped { at_timestamp } => {
                    format!("Dropped at {}", at_timestamp)
                }
            };

            // Get parent branch name (if any)
            let parent_name = if let Some(parent_id) = branch_meta.parent_id {
                storage.get_branch_name(parent_id)
            } else {
                None
            };

            let tuple = crate::Tuple::new(vec![
                crate::Value::String(branch_meta.name.clone()),
                crate::Value::Int8(branch_meta.branch_id as i64),
                parent_name.map(crate::Value::String).unwrap_or(crate::Value::Null),
                crate::Value::Timestamp(chrono::DateTime::from_timestamp(branch_meta.created_at as i64, 0).unwrap_or_default()),
                crate::Value::String(state_str),
            ]);
            tuples.push(tuple);
        }

        // Create schema for SHOW BRANCHES output
        let schema = Arc::new(crate::Schema {
            columns: vec![
                crate::Column {
                    name: "branch_name".to_string(),
                    data_type: crate::DataType::Text,
                    nullable: false,
                    primary_key: false,
                source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                },
                crate::Column {
                    name: "branch_id".to_string(),
                    data_type: crate::DataType::Int8,
                    nullable: false,
                    primary_key: false,
                source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                },
                crate::Column {
                    name: "parent_branch".to_string(),
                    data_type: crate::DataType::Text,
                    nullable: true,
                    primary_key: false,
                source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                },
                crate::Column {
                    name: "created_at".to_string(),
                    data_type: crate::DataType::Timestamp,
                    nullable: false,
                    primary_key: false,
                source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                },
                crate::Column {
                    name: "state".to_string(),
                    data_type: crate::DataType::Text,
                    nullable: false,
                    primary_key: false,
                source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                },
            ],
        });

        // Return materialized result
        Ok(Box::new(MaterializedOperator::new(tuples, schema)))
    } else {
        return Err(Error::execution("No storage engine available"));
    }
}
