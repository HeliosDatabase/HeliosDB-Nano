//! Phase 3 features: Branching, Materialized Views, and System Views
//!
//! This module handles execution of Phase 3 advanced database features.

#![allow(elided_lifetimes_in_paths)]

use super::scan::{MaterializedOperator, ScanOperator};
use super::{Executor, PhysicalOperator};
use crate::sql::LogicalPlan;
use crate::storage::{MvDeltaOperation, MvDeltaSet};
use crate::{Error, Result};
use std::sync::Arc;

/// Optimize a materialized-view query the same way the direct query path does
/// (`EmbeddedDatabase::query_with_columns`) before materializing it.
///
/// Without this, the MV path ran `plan_to_operator` on the *raw* (unoptimized)
/// plan, which undercounts aggregates at scale — issue #2 / Quirk J: on a
/// 448k-row table, `COUNT(DISTINCT session_id)` materialized as 4 instead of 265
/// (and `COUNT(*)+SUM` as ~407) because the optimized plan (notably projection
/// pruning) is what produces a correct full-table scan for the aggregate input.
fn optimize_view_query(plan: &LogicalPlan) -> Result<LogicalPlan> {
    use crate::optimizer;
    let stats = optimizer::cost::StatsCatalog::new();
    let rules: Vec<Box<dyn optimizer::rules::OptimizationRule>> = vec![
        Box::new(optimizer::rules::ConstantFoldingRule::new()),
        Box::new(optimizer::rules::SelectionPushdownRule::new()),
        Box::new(optimizer::rules::JoinPredicatePushdownRule::new()),
        Box::new(optimizer::rules::ProjectionPruningRule::new()),
        Box::new(optimizer::rules::StorageFilterPushdownRule::new()),
    ];
    let opt = optimizer::Optimizer::with_rules(stats, rules, optimizer::OptimizerConfig::default());
    opt.optimize_recursive(plan.clone())
}

/// Rebind the stored definition's physical scan layouts while retaining its
/// declared output. SELECT * expands when the view is defined, not on REFRESH.
/// Names/types must still exist; source storage-mode changes and appended columns
/// can be read with the current decoder without altering the view's columns.
fn rebind_view_layouts(plan: &mut LogicalPlan, storage: &crate::storage::StorageEngine) -> Result<()> {
    use crate::sql::{LogicalExpr as E, LogicalPlan as P};
    use std::collections::HashSet;
    fn expression(expr: &mut E, storage: &crate::storage::StorageEngine, ctes: &HashSet<String>) -> Result<()> {
        match expr {
            E::BinaryExpr { left, right, .. }
            | E::ArraySubscript {
                array: left,
                index: right,
            } => {
                expression(left, storage, ctes)?;
                expression(right, storage, ctes)?;
            }
            E::UnaryExpr { expr, .. } | E::Cast { expr, .. } | E::IsNull { expr, .. } | E::InSet { expr, .. } => {
                expression(expr, storage, ctes)?
            }
            E::AggregateFunction { args, .. } | E::ScalarFunction { args, .. } | E::Tuple { items: args } => {
                for arg in args {
                    expression(arg, storage, ctes)?;
                }
            }
            E::Case {
                expr,
                when_then,
                else_result,
            } => {
                if let Some(expr) = expr {
                    expression(expr, storage, ctes)?;
                }
                for (when, then) in when_then {
                    expression(when, storage, ctes)?;
                    expression(then, storage, ctes)?;
                }
                if let Some(expr) = else_result {
                    expression(expr, storage, ctes)?;
                }
            }
            E::Between { expr, low, high, .. } => {
                expression(expr, storage, ctes)?;
                expression(low, storage, ctes)?;
                expression(high, storage, ctes)?;
            }
            E::InList { expr, list, .. } => {
                expression(expr, storage, ctes)?;
                for value in list {
                    expression(value, storage, ctes)?;
                }
            }
            E::ScalarSubquery { subquery } | E::Exists { subquery, .. } => visit(subquery, storage, ctes)?,
            E::InSubquery { expr, subquery, .. } => {
                expression(expr, storage, ctes)?;
                visit(subquery, storage, ctes)?;
            }
            E::WindowFunction {
                args,
                partition_by,
                order_by,
                ..
            } => {
                for expr in args.iter_mut().chain(partition_by.iter_mut()) {
                    expression(expr, storage, ctes)?;
                }
                for (expr, _) in order_by {
                    expression(expr, storage, ctes)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn visit(plan: &mut P, storage: &crate::storage::StorageEngine, ctes: &HashSet<String>) -> Result<()> {
        match plan {
            P::Scan {
                table_name,
                alias,
                schema,
                projection,
                ..
            }
            | P::FilteredScan {
                table_name,
                alias,
                schema,
                projection,
                ..
            } => {
                if ctes.contains(table_name) {
                    return Ok(());
                }
                let current = storage.catalog().get_table_schema(table_name)?;
                // Scan predicates can contain physical positions in older plans.
                // Keep their original order; append/storage changes are safe.
                if current.columns.len() < schema.columns.len()
                    || !schema
                        .columns
                        .iter()
                        .zip(&current.columns)
                        .all(|(old, now)| old.name == now.name && old.data_type == now.data_type)
                {
                    return Err(Error::query_execution(format!("materialized view definition depends on columns of '{table_name}' whose names, positions or types changed; recreate the view")));
                }
                let original_output = if current.columns.len() != schema.columns.len() {
                    let positions = projection
                        .clone()
                        .unwrap_or_else(|| (0..schema.columns.len()).collect());
                    let mut output = Vec::with_capacity(positions.len());
                    for index in positions {
                        output.push(
                            schema
                                .columns
                                .get(index)
                                .ok_or_else(|| Error::storage("stored materialized-view projection is out of bounds"))?
                                .clone(),
                        );
                    }
                    let source = alias.as_ref().unwrap_or(table_name).clone();
                    // Use a real Project node, not just Scan.projection: all
                    // logical as well as physical operator schemas retain N
                    // columns when the source now has N+1 (including CTEs).
                    *projection = None;
                    Some((output, source))
                } else {
                    None
                };
                if original_output.is_some()
                    || schema
                        .columns
                        .iter()
                        .zip(&current.columns)
                        .any(|(old, now)| old.storage_mode != now.storage_mode)
                {
                    *schema = Arc::new(current);
                }
                if let P::FilteredScan {
                    predicate: Some(predicate),
                    ..
                } = plan
                {
                    expression(predicate, storage, ctes)?;
                }
                if let Some((columns, source)) = original_output {
                    let input = Box::new(std::mem::replace(plan, P::Noop));
                    *plan = P::Project {
                        input,
                        exprs: columns
                            .iter()
                            .map(|column| E::Column {
                                table: Some(source.clone()),
                                name: column.name.clone(),
                            })
                            .collect(),
                        aliases: columns.iter().map(|column| column.name.clone()).collect(),
                        distinct: false,
                        distinct_on: None,
                        source_alias: None,
                    };
                }
            }
            P::Filter { input, predicate } => {
                visit(input, storage, ctes)?;
                expression(predicate, storage, ctes)?;
            }
            P::Project {
                input,
                exprs,
                distinct_on,
                ..
            } => {
                visit(input, storage, ctes)?;
                for expr in exprs {
                    expression(expr, storage, ctes)?;
                }
                if let Some(exprs) = distinct_on {
                    for expr in exprs {
                        expression(expr, storage, ctes)?;
                    }
                }
            }
            P::Aggregate {
                input,
                group_by,
                aggr_exprs,
                having,
            } => {
                visit(input, storage, ctes)?;
                for expr in group_by.iter_mut().chain(aggr_exprs.iter_mut()) {
                    expression(expr, storage, ctes)?;
                }
                if let Some(expr) = having {
                    expression(expr, storage, ctes)?;
                }
            }
            P::Sort { input, exprs, .. } => {
                visit(input, storage, ctes)?;
                for expr in exprs {
                    expression(expr, storage, ctes)?;
                }
            }
            P::Limit { input, .. } => visit(input, storage, ctes)?,
            P::Join { left, right, on, .. } => {
                visit(left, storage, ctes)?;
                visit(right, storage, ctes)?;
                if let Some(expr) = on {
                    expression(expr, storage, ctes)?;
                }
            }
            P::Union { left, right, .. } | P::Intersect { left, right, .. } | P::Except { left, right, .. } => {
                visit(left, storage, ctes)?;
                visit(right, storage, ctes)?;
            }
            P::With {
                ctes: definitions,
                query,
                recursive,
            } => {
                let mut names = ctes.clone();
                // Match executor visibility: a nonrecursive definition can
                // still read a real table bearing its own/later sibling's name.
                for (name, definition, _) in definitions {
                    if *recursive {
                        names.insert(name.clone());
                    }
                    visit(definition, storage, &names)?;
                    names.insert(name.clone());
                }
                visit(query, storage, &names)?;
            }
            P::TableFunction { args, .. } => {
                for expr in args {
                    expression(expr, storage, ctes)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(plan, storage, &HashSet::new())
}

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
        LogicalPlan::CreateBranch {
            branch_name,
            if_not_exists,
            parent,
            as_of,
            options,
        } => handle_create_branch(executor, branch_name, *if_not_exists, parent, as_of, options),
        LogicalPlan::DropBranch { branch_name, if_exists } => handle_drop_branch(executor, branch_name, *if_exists),
        LogicalPlan::MergeBranch {
            source,
            target,
            options,
        } => handle_merge_branch(executor, source, target, options),
        LogicalPlan::UseBranch { branch_name } => handle_use_branch(executor, branch_name),
        LogicalPlan::ShowBranches => handle_show_branches(executor),
        LogicalPlan::CreateMaterializedView {
            name,
            query,
            options,
            if_not_exists,
        } => handle_create_materialized_view(executor, name, query, options, *if_not_exists),
        LogicalPlan::RefreshMaterializedView {
            name,
            concurrent,
            incremental,
        } => handle_refresh_materialized_view(executor, name, *concurrent, *incremental),
        LogicalPlan::DropMaterializedView { name, if_exists } => {
            handle_drop_materialized_view(executor, name, *if_exists)
        }
        LogicalPlan::AlterMaterializedView { name, options } => handle_alter_materialized_view(executor, name, options),
        LogicalPlan::SystemView { name, .. } => handle_system_view(executor, name),
        LogicalPlan::CreateView {
            name,
            query_sql,
            if_not_exists,
            or_replace,
            creator_schema,
        } => handle_create_view(
            executor,
            name,
            query_sql,
            *if_not_exists,
            *or_replace,
            creator_schema.clone(),
        ),
        LogicalPlan::DropView { name, if_exists } => handle_drop_view(executor, name, *if_exists),
        _ => Err(Error::query_execution("Unsupported advanced operation")),
    }
}

/// Handle CREATE BRANCH
fn handle_create_branch(
    executor: &Executor,
    branch_name: &str,
    if_not_exists: bool,
    parent: &Option<String>,
    as_of: &crate::sql::logical_plan::AsOfClause,
    options: &[crate::sql::logical_plan::BranchOption],
) -> Result<Box<dyn PhysicalOperator>> {
    use crate::storage::BranchOptions;

    tracing::info!(
        "Executing CREATE BRANCH {} (IF NOT EXISTS: {}) FROM {:?} AS OF {:?} WITH {:?}",
        branch_name,
        if_not_exists,
        parent,
        as_of,
        options
    );

    if let Some(storage) = executor.storage() {
        // sprinter 6bc4a4040eb7 (B1): "exists" means ACTIVE — the same
        // definition the binding resolver uses. A dropped branch's name is free:
        // `IF NOT EXISTS` over it creates a fresh branch (new id, none of the
        // old rows) instead of no-opping onto the dropped branch's overlay.
        if if_not_exists && storage.active_branch_exists(branch_name) {
            tracing::info!("Branch '{}' already exists (IF NOT EXISTS specified)", branch_name);
            return Ok(empty_phase3_result(executor));
        }

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
        let mut _gc_pin = None;
        let snapshot_id = match as_of {
            crate::sql::logical_plan::AsOfClause::Now => {
                // Use current timestamp (latest snapshot)
                None
            }
            other_as_of => {
                // Resolve timestamp/transaction/SCN to snapshot ID.
                // R4.3: resolve_as_of errors when the anchor lies below the
                // version-GC low watermark (history already collected), and
                // the pin guard keeps GC from advancing past the anchor
                // until the branch is created. Sprinter 6bc4a4040eb7 (B4): from
                // then on it holds what it needs in its own overlay, and pins
                // nothing.
                let snapshot_manager = storage.snapshot_manager();
                let resolved_snapshot = snapshot_manager.resolve_as_of(other_as_of)?;
                _gc_pin = Some(storage.pin_historical_snapshot(resolved_snapshot)?);

                tracing::debug!("Resolved AS OF {:?} to snapshot ID {}", other_as_of, resolved_snapshot);

                Some(resolved_snapshot)
            }
        };

        // Resolve parent branch:
        // - If explicitly specified, use that
        // - Otherwise, the branch THIS SESSION is on (sprinter 6bc4a4040eb7, B1
        //   — it was the engine-wide selector, i.e. whichever branch some other
        //   connection last switched to). A binding whose branch has since gone
        //   is refused rather than read as main.
        let resolved_parent = match parent {
            Some(p) => Some(p.clone()),
            None => storage.check_active_branch()?.map(|binding| binding.name.clone()),
        };

        // Create the branch at the resolved snapshot
        let branch_id =
            storage.create_branch_at_snapshot(branch_name, resolved_parent.as_deref(), snapshot_id, branch_opts)?;

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
    Ok(empty_phase3_result(executor))
}

fn empty_phase3_result(executor: &Executor) -> Box<dyn PhysicalOperator> {
    Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    )
}

/// Handle DROP BRANCH
fn handle_drop_branch(executor: &Executor, branch_name: &str, if_exists: bool) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!("Executing DROP BRANCH {} (IF EXISTS: {})", branch_name, if_exists);

    if let Some(storage) = executor.storage() {
        // sprinter 6bc4a4040eb7 (B1): PostgreSQL refuses to drop the database
        // the session is connected to (55006 `cannot drop the currently open
        // database`); a branch is refused the same way for the session bound to
        // it. Other sessions bound to it do not block the drop — they fail
        // closed on their next statement instead.
        if storage
            .active_branch()
            .is_some_and(|binding| binding.name == branch_name)
        {
            return Err(Error::branch_in_use(branch_name));
        }
        // Drop the branch
        storage.drop_branch(branch_name, if_exists)?;

        tracing::info!("Successfully dropped branch '{}'", branch_name);
    } else {
        return Err(Error::execution("No storage engine available"));
    }

    // Return empty result set for DDL
    Ok(Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    ))
}

/// Handle MERGE BRANCH
fn handle_merge_branch(
    executor: &Executor,
    source: &str,
    target: &str,
    options: &[crate::sql::logical_plan::MergeOption],
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!("Executing MERGE BRANCH {} INTO {} WITH {:?}", source, target, options);

    if let Some(storage) = executor.storage() {
        // `conflict_resolution` parses and maps to a MergeStrategy, and
        // `StorageEngine::merge_branch` then ignores it: that function takes
        // `_strategy` and returns a hard-coded `conflicts: Vec::new()`, so it
        // never detects a conflict and never applies a strategy. Merging is
        // last-writer-wins.
        //
        // Accepting the option would therefore be a silent no-op — the caller
        // asks for 'target_wins', gets 'branch_wins', and is told the merge
        // completed with 0 conflicts. Fail loudly instead until conflict
        // detection exists. `delete_branch_after` IS honoured (below) and stays
        // accepted.
        if options
            .iter()
            .any(|o| matches!(o, crate::sql::logical_plan::MergeOption::ConflictResolution(_)))
        {
            return Err(Error::execution(
                "MERGE BRANCH conflict_resolution is not implemented: merges are last-writer-wins \
                 and conflicts are never detected. Remove the option to merge with those semantics.",
            ));
        }

        // Determine merge strategy from options
        let strategy = resolve_merge_strategy(options);

        // Perform the merge. Sprinter 6bc4a4040eb7 (B4 fix pass): a drop that would be
        // refused is refused before the merge lands, under the merge's close of the
        // source, where no child can appear.
        let result = if should_delete_branch_after_merge(options) {
            storage.merge_branch_to_drop_source(source, target, strategy)?
        } else {
            storage.merge_branch(source, target, strategy)?
        };

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
    Ok(Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    ))
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
        name,
        if_not_exists,
        options
    );

    // First check: verify storage exists and view doesn't already exist
    // We do this separately to avoid borrow conflicts
    {
        let storage = executor
            .storage()
            .ok_or_else(|| Error::execution("No storage engine available"))?;
        // sprinter 6bc4a4040eb7 (B2 fix pass): a materialized view lives in
        // main's catalog and its rows in main's data table; until phase B3
        // scopes it to a branch, creating one from a branch is refused before
        // anything is registered.
        storage.refuse_statement_on_branch("CREATE MATERIALIZED VIEW")?;
        let mv_catalog = storage.mv_catalog();

        // Check if view already exists
        if mv_catalog.view_exists(name)? {
            if if_not_exists {
                tracing::info!("Materialized view '{}' already exists (IF NOT EXISTS specified)", name);
                return Ok(Box::new(
                    ScanOperator::new(
                        "".to_string(),
                        Arc::new(crate::Schema { columns: vec![] }),
                        None,
                        vec![],
                        vec![],
                    )
                    .with_timeout(executor.timeout_ctx()),
                ));
            } else {
                return Err(Error::query_execution(format!(
                    "Materialized view '{}' already exists",
                    name
                )));
            }
        }
    } // Storage borrow ends here

    // Sprinter 6bc4a4040eb7 (B4 fix pass): before anything is registered: a name the
    // data table cannot have, and a statement that registers a view and fills its table
    // in several writes, wholly before a fork of main or wholly after it.
    crate::storage::Catalog::refuse_unstorable_table_name(name)?;
    let _main_ddl = executor
        .storage()
        .ok_or_else(|| Error::execution("No storage engine available"))?
        .enter_main_statement(
            &[crate::storage::MaterializedViewCatalog::mv_data_table_name(name).as_str()],
            "CREATE MATERIALIZED VIEW",
        )?;

    // Optimize the view query the same way direct queries are (issue #2 / Quirk J):
    // the raw plan undercounts aggregates at scale. Materialize AND persist the
    // optimized plan so REFRESH stays correct too.
    let optimized_query = optimize_view_query(query)?;
    // GH#29 (c3): the plan is persisted below as bincode, which cannot carry
    // `Project::source_alias` (the stamp a derived table's / view's alias
    // resolves through at runtime). Rewrite it first so it needs no stamp —
    // every unshadowed `alias.col` becomes the bare `col`, every stamp is
    // cleared — and refuse (0A000, workaround named) only the shadowed shape
    // the rewrite cannot express. The SAME rewritten plan is materialized
    // below and stored, so CREATE and REFRESH execute byte-identical plans:
    // a shape the rewrite gets wrong fails here, never at a later refresh.
    let optimized_query = optimized_query.destamp_source_aliases()?;
    // Materialize against the current (branch-aware) view via a FRESH executor — NOT
    // the CREATE statement's executor/transaction. Issue #2 / Quirk J: the inherited
    // transaction's snapshot under-counts at scale (COUNT(DISTINCT) materialized as 4
    // instead of 265 on a 448k-row table; COUNT(*)+SUM as ~407), because the
    // materialization ran under the DDL statement's transaction snapshot rather than
    // the full current table. A fresh executor with no active txn scans the whole
    // table, exactly like a direct autocommit query.
    let storage_arc = executor
        .storage()
        .ok_or_else(|| Error::execution("No storage engine available"))?
        .clone();
    let mut mat_exec = Executor::with_storage(&storage_arc);
    let mut query_operator = mat_exec.plan_to_operator(&optimized_query)?;
    let schema = query_operator.schema();

    // Extract base tables from the query (simplified - just look at Scan nodes)
    let base_tables = extract_base_tables(query);

    // Serialize the OPTIMIZED query plan for re-execution during REFRESH
    let query_plan_bytes = bincode::serialize(&optimized_query)
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

    // Process WITH options.
    //
    // CONTRACT: the auto-refresh runtime gate reads `metadata.metadata["auto_refresh"]`
    // (storage/mv_auto_refresh.rs `perform_staleness_check`), and so does the status view
    // (storage/mv_system_views.rs). `metadata.refresh_strategy` is DISPLAY-ONLY — it is
    // surfaced by `pg_matviews` and nothing gates on it. Writing only the strategy (what
    // this loop used to do) meant `WITH (auto_refresh = true)` never enabled anything.
    //
    // Every option now lands in the metadata map so ALTER round-trips and system views
    // stay honest; the ones with no consumer yet are recorded as such below.
    use crate::sql::logical_plan::MaterializedViewOption as Opt;
    for option in options {
        match option {
            Opt::AutoRefresh(enabled) => {
                // Stored explicitly (including "false") so the value round-trips rather
                // than relying on the reader's `unwrap_or(false)` for absence.
                metadata
                    .metadata
                    .insert("auto_refresh".to_string(), enabled.to_string());
                if *enabled {
                    metadata.refresh_strategy = "auto".to_string();
                }
            }
            // Stored, but throttling is governed by the GLOBAL `AutoRefreshConfig` /
            // `SchedulerConfig` CPU limits — there is no per-view CPU gate yet.
            Opt::MaxCpuPercent(pct) => {
                metadata.metadata.insert("max_cpu_percent".to_string(), pct.to_string());
            }
            // Stored; staleness scheduling is time-based, so no consumer yet.
            Opt::ThresholdDmlRate(rate) => {
                metadata
                    .metadata
                    .insert("threshold_dml_rate".to_string(), rate.to_string());
            }
            Opt::ThresholdTableSize(v) => {
                metadata.metadata.insert("threshold_table_size".to_string(), v.clone());
            }
            Opt::LazyUpdate(b) => {
                metadata.metadata.insert("lazy_update".to_string(), b.to_string());
            }
            Opt::LazyCatchupWindow(v) => {
                metadata.metadata.insert("lazy_catchup_window".to_string(), v.clone());
            }
            Opt::Distribution(v) => {
                metadata.metadata.insert("distribution".to_string(), v.clone());
            }
            Opt::ReplicationFactor(rf) => {
                metadata
                    .metadata
                    .insert("replication_factor".to_string(), rf.to_string());
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
        let storage = executor
            .storage()
            .ok_or_else(|| Error::execution("No storage engine available"))?;
        let mv_catalog = storage.mv_catalog();

        // The definition, populated table, refreshed record and all logical WAL
        // entries become durable together. A failure publishes no partial view.
        let row_count = mv_catalog.create_view_with_data(metadata, &tuples)?;

        // Initialize delta tracking for base tables
        // This will enable incremental refresh for this materialized view
        tracing::debug!("Initializing delta tracking for base tables: {:?}", base_tables);
        for table_name in &base_tables {
            tracing::debug!(
                "Delta tracking enabled for table '{}' (used by MV '{}')",
                table_name,
                name
            );
            // Note: Delta tracking is automatically handled by the MvDeltaTracker
            // which captures all table changes. No explicit registration needed here.
        }

        tracing::info!(
            "Successfully created materialized view '{}' with {} rows (delta tracking active)",
            name,
            row_count
        );
    }

    // Return empty result set for DDL
    Ok(Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    ))
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
        name,
        concurrent,
        incremental_requested
    );

    // Sprinter 6bc4a4040eb7 (B4 fix pass): a refresh replaces the view's rows in several
    // writes, so it is wholly before a fork of main or wholly after it. Here, not in the
    // statement funnels: the auto-refresh worker gets here too, with no statement of its
    // own. On a branch it is refused just below, before it could write.
    let _main_ddl = {
        let storage = executor
            .storage()
            .ok_or_else(|| Error::execution("No storage engine available"))?;
        storage.refuse_statement_on_branch("REFRESH MATERIALIZED VIEW")?;
        storage.enter_main_statement(
            &[crate::storage::MaterializedViewCatalog::mv_data_table_name(name).as_str()],
            "REFRESH MATERIALIZED VIEW",
        )?
    };

    // Phase 1: Read metadata from storage
    let (metadata, delta_count, use_incremental, total_deltas) = {
        let storage = executor
            .storage()
            .ok_or_else(|| Error::execution("No storage engine available"))?;
        // sprinter 6bc4a4040eb7 (B2 fix pass): REFRESH drops, purges and
        // recreates the view's MAIN data table and re-inserts through the
        // session's branch — on a branch it emptied main's view. Refused until
        // phase B3 scopes materialized views, before anything is read or
        // written.
        storage.refuse_statement_on_branch("REFRESH MATERIALIZED VIEW")?;
        let mv_catalog = storage.mv_catalog();

        // Get view metadata
        let metadata = mv_catalog.get_view(name)?;
        tracing::debug!(
            "Refreshing materialized view '{}' with query: {}",
            name,
            metadata.query_text
        );

        // Check if incremental refresh is possible
        let can_use_incremental = metadata.last_refresh.is_some() && !concurrent;

        // Check delta count for incremental refresh decision
        let delta_count = if can_use_incremental {
            if let Some(last_refresh) = metadata.last_refresh {
                let delta_tracker = storage.mv_delta_tracker();
                delta_tracker
                    .count_deltas_since(&metadata.base_tables, last_refresh)
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
                                table_name,
                                inserts,
                                updates,
                                deletes
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
        let storage = executor
            .storage()
            .ok_or_else(|| Error::execution("No storage engine available"))?;
        let mv_catalog = storage.mv_catalog();
        let mut updated_metadata = metadata.clone();
        updated_metadata.mark_refreshed(metadata.row_count.unwrap_or(0));
        mv_catalog.refresh_metadata_in_one_group(&updated_metadata, concurrent, incremental_requested)?;

        return Ok(Box::new(ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )));
    }

    // Phase 2: Execute query (requires mutable borrow of executor)
    let mut query_plan = metadata.get_query_plan()?;
    rebind_view_layouts(
        &mut query_plan,
        executor
            .storage()
            .ok_or_else(|| Error::execution("No storage engine available"))?,
    )?;
    let schema = Arc::new(metadata.schema.clone());

    if use_incremental {
        tracing::info!("Incremental refresh for MV '{}': {} deltas to apply", name, delta_count);
    } else if delta_count > 0 {
        tracing::info!(
            "Full refresh for MV '{}': {} deltas (threshold exceeded or concurrent mode)",
            name,
            delta_count
        );
    } else {
        tracing::info!("Full refresh for MV '{}'", name);
    }

    // Re-materialize against the current branch-aware view via a FRESH executor —
    // not the REFRESH statement's transaction snapshot, which under-counts at scale
    // (issue #2 / Quirk J; see the CREATE path for the full diagnosis).
    let storage_arc = executor
        .storage()
        .ok_or_else(|| Error::execution("No storage engine available"))?
        .clone();
    let mut mat_exec = Executor::with_storage(&storage_arc);
    let mut query_operator = mat_exec.plan_to_operator(&query_plan)?;
    let mut tuples = Vec::new();
    while let Some(tuple) = query_operator.next()? {
        tuples.push(tuple);
    }
    let row_count = tuples.len() as u64;

    // Phase 3: Store results (re-borrow storage)
    {
        let storage = executor
            .storage()
            .ok_or_else(|| Error::execution("No storage engine available"))?;
        let mv_catalog = storage.mv_catalog();

        // The refreshed record, marked before the write that carries it.
        let mut refreshed = metadata;
        if use_incremental {
            refreshed.delta_count_since_full = 0;
        }
        refreshed.mark_refreshed(row_count);

        // GH#36: the refresh in the logical WAL (the rows have entries of their own);
        // spec R5.3: in the refresh's own write group.
        let refresh_entry = crate::storage::WalOperation::RefreshMaterializedView {
            name: name.to_string(),
            concurrent,
            incremental: incremental_requested,
        };
        // Spec R5.3: ONE write group for every row deleted, every row inserted, the
        // refreshed record and the entries. A reader never sees an empty or a partial
        // view, which is also what CONCURRENTLY promises, so it takes the same path;
        // the temporary-table swap is left only for a data table this path cannot
        // rewrite in place (see `refresh_view_in_one_group`).
        mv_catalog.refresh_view_in_one_group(name, &tuples, &schema, &refreshed, refresh_entry)?;

        if use_incremental {
            // Applied-delta cleanup rides the refresh's own group too.
            tracing::info!(
                "Incremental refresh completed for MV '{}': {} rows (processed {} deltas)",
                name,
                row_count,
                total_deltas
            );
        } else {
            tracing::info!(
                "Successfully refreshed materialized view '{}' with {} rows{}",
                name,
                row_count,
                if concurrent {
                    " (CONCURRENT mode - zero downtime)"
                } else {
                    ""
                }
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
    tracing::info!("DROP MATERIALIZED VIEW {} (IF EXISTS: {})", name, if_exists);

    if let Some(storage) = executor.storage() {
        let mv_catalog = storage.mv_catalog();

        // Check if view exists
        if !mv_catalog.view_exists(name)? {
            if if_exists {
                tracing::info!("Materialized view '{}' does not exist (IF EXISTS specified)", name);
                return Ok(Box::new(
                    ScanOperator::new(
                        "".to_string(),
                        Arc::new(crate::Schema { columns: vec![] }),
                        None,
                        vec![],
                        vec![],
                    )
                    .with_timeout(executor.timeout_ctx()),
                ));
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
    Ok(Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    ))
}

/// Handle ALTER MATERIALIZED VIEW
fn handle_alter_materialized_view(
    executor: &Executor,
    name: &str,
    options: &std::collections::HashMap<String, String>,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!("ALTER MATERIALIZED VIEW {} SET {:?}", name, options);

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
                    metadata
                        .metadata
                        .insert("staleness_threshold".to_string(), value.clone());
                }
                "max_cpu_percent" => {
                    metadata.metadata.insert("max_cpu_percent".to_string(), value.clone());
                }
                "priority" => {
                    metadata.metadata.insert("priority".to_string(), value.clone());
                }
                // Explicit arm: this used to fall through to the `_` catch-all, which
                // happened to write the right key but never touched `refresh_strategy`,
                // so `pg_matviews` disagreed with the runtime gate.
                "auto_refresh" => {
                    let enabled = value.to_lowercase();
                    metadata.metadata.insert("auto_refresh".to_string(), enabled.clone());
                    if enabled == "true" {
                        metadata.refresh_strategy = "auto".to_string();
                    } else if metadata.refresh_strategy == "auto" {
                        metadata.refresh_strategy = "manual".to_string();
                    }
                }
                // The documented spelling. Keep the runtime gate in sync with it, so
                // `SET (refresh_strategy = 'auto')` actually enables auto-refresh
                // instead of only relabelling the view.
                "refresh_strategy" => {
                    metadata.refresh_strategy = value.clone();
                    let auto = value.eq_ignore_ascii_case("auto");
                    metadata.metadata.insert("auto_refresh".to_string(), auto.to_string());
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
    Ok(Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    ))
}

/// Handle SYSTEM VIEW query
fn handle_system_view(executor: &Executor, name: &str) -> Result<Box<dyn PhysicalOperator>> {
    // Execute system view query
    use crate::sql::phase3::SystemViewRegistry;

    let storage = executor
        .storage()
        .ok_or_else(|| Error::query_execution("System views require storage engine"))?;

    let registry = SystemViewRegistry::new();

    // Execute the system view and get results
    let tuples = registry.execute(name, storage)?;

    // Get schema for the view
    let schema = registry
        .get_schema(name)
        .ok_or_else(|| Error::query_execution(format!("System view '{}' schema not found", name)))?
        .clone();

    // Convert tuples to a materialized operator
    // Since we have all the data, we can return a simple in-memory result set
    Ok(Box::new(MaterializedOperator::new(tuples, Arc::new(schema))))
}

/// Resolve merge strategy from SQL options
fn resolve_merge_strategy(options: &[crate::sql::logical_plan::MergeOption]) -> crate::storage::MergeStrategy {
    use crate::sql::logical_plan::{ConflictResolution, MergeOption};
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
fn handle_use_branch(executor: &Executor, branch_name: &str) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!("Executing USE BRANCH {}", branch_name);

    if let Some(storage) = executor.storage() {
        // sprinter 6bc4a4040eb7 (B1): bind THIS session (the one whose
        // statement runs on this thread) to the branch — resolved as ACTIVE, so
        // an unknown, dropped or merged branch is refused 3D000 — and nobody
        // else. `main` returns the session to main.
        storage.use_branch(branch_name)?;

        tracing::debug!("Session switched to branch '{}'", branch_name);
    } else {
        return Err(Error::execution("No storage engine available"));
    }

    // Return empty result set
    Ok(Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    ))
}

/// Handle SHOW BRANCHES
fn handle_show_branches(executor: &Executor) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!("Executing SHOW BRANCHES");

    if let Some(storage) = executor.storage() {
        // Get all branches
        let branches = storage.list_branches_for_catalog()?;

        tracing::debug!("Found {} branches", branches.len());

        // Convert to tuples
        let mut tuples = Vec::new();
        for branch_meta in branches {
            // Format state as string
            let state_str = match branch_meta.state {
                crate::storage::BranchState::Active => "Active".to_string(),
                crate::storage::BranchState::Merged {
                    into_branch,
                    at_timestamp,
                } => {
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
                crate::Value::Timestamp(
                    chrono::DateTime::from_timestamp(branch_meta.created_at as i64, 0).unwrap_or_default(),
                ),
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
        Err(Error::execution("No storage engine available"))
    }
}

/// Handle CREATE VIEW (regular, non-materialized view)
fn handle_create_view(
    executor: &mut Executor,
    name: &str,
    query_sql: &str,
    if_not_exists: bool,
    or_replace: bool,
    creator_schema: Option<String>,
) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!(
        "CREATE{}VIEW {} (IF NOT EXISTS: {})",
        if or_replace { " OR REPLACE " } else { " " },
        name,
        if_not_exists
    );

    let storage = executor
        .storage()
        .ok_or_else(|| Error::execution("No storage engine available"))?;

    // Parse the query to get the schema
    let parser = crate::sql::Parser::new();
    let stmt = parser.parse_one(query_sql)?;

    // Plan the query to get the schema. Plan under the CREATING session's schema
    // so a body that references a schema-local table (bare name under a
    // non-`public` `search_path`) derives the correct result columns — the same
    // schema the body will later bind to at read time.
    let catalog = storage.catalog();
    let planner = crate::sql::Planner::with_catalog(&catalog).with_current_schema(creator_schema.clone());
    let plan = planner.statement_to_plan(stmt)?;
    let schema = (*plan.schema()).clone();

    // Create view metadata, capturing the creator's schema so the body binds at
    // CREATE (not per-reader).
    let metadata = crate::storage::ViewMetadata::new(name.to_string(), query_sql.to_string(), schema)
        .with_or_replace(or_replace)
        .with_creator_schema(creator_schema);

    // Store the view
    let view_catalog = storage.view_catalog();
    view_catalog.create_view(metadata, if_not_exists, or_replace)?;

    tracing::info!("Successfully created view '{}'", name);

    // Return empty result set for DDL
    Ok(Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    ))
}

/// Handle DROP VIEW
fn handle_drop_view(executor: &Executor, name: &str, if_exists: bool) -> Result<Box<dyn PhysicalOperator>> {
    tracing::info!("DROP VIEW {} (IF EXISTS: {})", name, if_exists);

    let storage = executor
        .storage()
        .ok_or_else(|| Error::execution("No storage engine available"))?;

    // Drop the view
    let view_catalog = storage.view_catalog();
    view_catalog.drop_view(name, if_exists)?;

    tracing::info!("Successfully dropped view '{}'", name);

    // Return empty result set for DDL
    Ok(Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    ))
}
