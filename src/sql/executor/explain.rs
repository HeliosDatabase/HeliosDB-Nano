//! EXPLAIN query plan executor
//!
//! This module handles SQL EXPLAIN statements with full feature support:
//! - PostgreSQL-compatible options (ANALYZE, VERBOSE, FORMAT, COSTS, etc.)
//! - HeliosDB extensions (STORAGE, AI, WHY_NOT, INDEXES, STATISTICS)
//!
//! # SQL Syntax
//!
//! ```sql
//! -- PostgreSQL-compatible
//! EXPLAIN SELECT * FROM users;
//! EXPLAIN ANALYZE SELECT * FROM users WHERE id = 1;
//! EXPLAIN (ANALYZE, VERBOSE) SELECT * FROM users;
//! EXPLAIN (FORMAT JSON) SELECT * FROM users;
//!
//! -- HeliosDB Extensions
//! EXPLAIN (STORAGE) SELECT * FROM orders;
//! EXPLAIN (AI) SELECT * FROM users WHERE status = 'active';
//! EXPLAIN (ANALYZE, STORAGE, WHY_NOT) SELECT * FROM orders;
//! ```

#![allow(elided_lifetimes_in_paths)]

use super::scan::MaterializedOperator;
use super::{Executor, PhysicalOperator};
use crate::sql::explain::{ExplainOutput, ExplainPlanner};
use crate::sql::explain_options::{ExplainFormatOption, ExplainOptions};
use crate::sql::explain_storage::format_storage_features_text;
use crate::sql::LogicalPlan;
use crate::{Column, DataType, Result, Schema, Tuple, Value};
use std::sync::Arc;
use std::time::Instant;

/// Handle EXPLAIN logical plan node with full options support
///
/// This function processes EXPLAIN statements using the enhanced ExplainPlanner
/// to provide comprehensive query plan analysis including:
/// - Cost and cardinality estimates
/// - Storage layer features (bloom filters, zone maps, compression, etc.)
/// - AI-powered explanations (when enabled)
/// - Why-Not analysis (why optimizations weren't applied)
/// - Multiple output formats (TEXT, JSON, YAML, TREE)
pub(super) fn handle_explain(
    executor: &Executor,
    plan: &LogicalPlan,
    options: &ExplainOptions,
) -> Result<Box<dyn PhysicalOperator>> {
    // Create schema for EXPLAIN output (single text column called "QUERY PLAN")
    let schema = Arc::new(Schema {
        columns: vec![Column {
            name: "QUERY PLAN".to_string(),
            data_type: DataType::Text,
            nullable: false,
            primary_key: false,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
        }],
    });

    // Create ExplainPlanner with appropriate mode and format
    let mode = options.to_explain_mode();
    let format = options.to_explain_format();

    let explain_planner = ExplainPlanner::new(mode, format);
    // Note: Storage statistics require Arc<StorageEngine> which we don't have here.
    // The basic explain functionality works without storage statistics.

    // Generate explain output from the plan
    let mut output = explain_planner.explain(plan)?;

    // R4.4: annotate scan nodes the executor will serve as index range scans
    // so EXPLAIN shows the access path that actually runs.
    annotate_index_range_scans(executor, plan, &mut output.plan);

    // Storage features not available in this execution context
    // (would require Arc<StorageEngine> instead of &StorageEngine)
    let storage_features = Vec::new();

    // Execute query for ANALYZE mode
    let (actual_rows, actual_time_ms, execution_error) = if options.analyze {
        execute_for_analyze(executor, plan)
    } else {
        (None, None, None)
    };

    // Update output with execution results
    if let Some(rows) = actual_rows {
        output.actual_rows = Some(rows);
    }
    if let Some(time_ms) = actual_time_ms {
        output.actual_time_ms = Some(time_ms);
    }
    if let Some(error) = execution_error {
        output.execution_error = Some(error);
    }

    // Format the output based on the requested format
    let formatted_output = match options.format {
        ExplainFormatOption::Json => format_json_output(&output, &storage_features, options),
        ExplainFormatOption::Yaml => format_yaml_output(&output, &storage_features, options),
        ExplainFormatOption::Tree | ExplainFormatOption::Text => {
            format_text_output(&output, &storage_features, options, &explain_planner)
        }
    };

    // Convert formatted output to tuples (one line per row)
    let tuples: Vec<Tuple> = formatted_output
        .lines()
        .map(|line| Tuple::new(vec![Value::String(line.to_string())]))
        .collect();

    Ok(Box::new(MaterializedOperator::new(tuples, schema)))
}

/// R4.4: walk the logical plan and the rendered `PlanNode` tree in lockstep,
/// rewriting nodes whose predicate qualifies for the index range scan fast
/// path (`scan::indexed_range_lookup`) so EXPLAIN surfaces it.
fn annotate_index_range_scans(executor: &Executor, plan: &LogicalPlan, node: &mut crate::sql::explain::PlanNode) {
    use crate::sql::explain::PlanNode;

    fn annotate(node: &mut PlanNode, table_name: &str, spec: &super::scan::IndexRangeSpec) {
        node.node_type = "IndexRangeScan".to_string();
        node.operation = format!(
            "Index Range Scan using {} on {} ({})",
            spec.index_name, table_name, spec.display
        );
        node.details.insert("index".to_string(), spec.index_name.clone());
        node.details.insert("range".to_string(), spec.display.clone());
    }

    fn annotate_point(node: &mut PlanNode, table_name: &str, index_name: &str) {
        node.node_type = "IndexPointLookup".to_string();
        node.operation = format!("Index Point Lookup using {} on {}", index_name, table_name);
        node.details.insert("index".to_string(), index_name.to_string());
    }

    fn annotate_in_list(node: &mut PlanNode, table_name: &str, index_name: &str, n: usize) {
        node.node_type = "IndexInListProbe".to_string();
        node.operation = format!("Index IN Probe ({n} values) using {index_name} on {table_name}");
        node.details.insert("index".to_string(), index_name.to_string());
    }

    let common_gates_pass = |table_name: &str| -> Option<&crate::storage::StorageEngine> {
        let storage = executor.storage()?;
        if storage.is_branch_active() {
            return None;
        }
        // Mirror the executor's gates (transaction snapshot, CTE shadowing,
        // materialized views, real-table check) so the displayed plan is the
        // plan that would actually execute.
        if executor.txn_forces_slow_reads_for_table(table_name)
            || executor.get_cte(table_name).is_some()
            || storage.mv_catalog().view_exists(table_name).unwrap_or(true)
            || !storage.catalog().table_exists(table_name).unwrap_or(false)
        {
            return None;
        }
        Some(storage)
    };

    // Mirrors `try_index_point_lookup_for_scan` (minus subquery
    // materialization — EXPLAIN must not execute subqueries, so `col = (…)`
    // predicates conservatively render unannotated).
    let point_spec = |table_name: &str, schema: &crate::Schema, predicate: &crate::sql::LogicalExpr| {
        let storage = common_gates_pass(table_name)?;
        super::scan::indexed_equality_lookup(storage, table_name, schema, predicate, executor.parameters())
            .map(|(index_name, _)| index_name)
    };

    let in_list_spec = |table_name: &str, schema: &crate::Schema, predicate: &crate::sql::LogicalExpr| {
        let storage = common_gates_pass(table_name)?;
        super::scan::indexed_in_list_lookup(storage, table_name, schema, predicate, executor.parameters())
            .map(|(index_name, values)| (index_name, values.len()))
    };

    let range_spec = |table_name: &str, schema: &crate::Schema, predicate: &crate::sql::LogicalExpr| {
        if super::scan::index_range_fast_path_disabled() {
            return None;
        }
        let storage = common_gates_pass(table_name)?;
        let spec = super::scan::indexed_range_lookup(storage, table_name, schema, predicate, executor.parameters())?;
        // Selectivity guard: ranges the executor would hand back to the
        // sequential scan must not be advertised as index range scans.
        super::scan::guarded_range_pairs(storage, &spec)?;
        Some(spec)
    };

    match plan {
        LogicalPlan::Filter { input, predicate } => {
            if let LogicalPlan::Scan {
                table_name,
                schema,
                as_of: None,
                ..
            } = input.as_ref()
            {
                // Same priority as the executor: point → IN-list → range.
                if let Some(index_name) = point_spec(table_name, schema, predicate) {
                    annotate_point(node, table_name, &index_name);
                    return;
                }
                if let Some((index_name, n)) = in_list_spec(table_name, schema, predicate) {
                    annotate_in_list(node, table_name, &index_name, n);
                    return;
                }
                if let Some(spec) = range_spec(table_name, schema, predicate) {
                    annotate(node, table_name, &spec);
                    return;
                }
            }
            if let Some(child) = node.children.first_mut() {
                annotate_index_range_scans(executor, input, child);
            }
        }
        LogicalPlan::FilteredScan {
            table_name,
            schema,
            predicate: Some(predicate),
            as_of: None,
            ..
        } => {
            if let Some(index_name) = point_spec(table_name, schema, predicate) {
                annotate_point(node, table_name, &index_name);
            } else if let Some((index_name, n)) = in_list_spec(table_name, schema, predicate) {
                annotate_in_list(node, table_name, &index_name, n);
            } else if let Some(spec) = range_spec(table_name, schema, predicate) {
                annotate(node, table_name, &spec);
            }
        }
        LogicalPlan::Limit { input, .. } => {
            // R4.4: `ORDER BY indexed_col ASC LIMIT k` served by ordered
            // index iteration — rewrite the rendered Sort node the fast path
            // eliminates (same detection the executor runs; display only).
            if let Ok(Some(spec)) = executor.index_ordered_topk_detect(input) {
                if let Some(sort_node) = find_node_mut(node, "Sort") {
                    sort_node.node_type = "IndexOrderedScan".to_string();
                    sort_node.operation = format!(
                        "Index Ordered Scan using {} on {} (ORDER BY {} ASC, no sort)",
                        spec.index_name,
                        spec.table_name(),
                        spec.column_name
                    );
                    sort_node.details.insert("index".to_string(), spec.index_name.clone());
                    sort_node
                        .details
                        .insert("order".to_string(), format!("{} ASC", spec.column_name));
                    return;
                }
            }
            if let Some(child) = node.children.first_mut() {
                annotate_index_range_scans(executor, input, child);
            }
        }
        LogicalPlan::Project { input, .. } | LogicalPlan::Aggregate { input, .. } | LogicalPlan::Sort { input, .. } => {
            if let Some(child) = node.children.first_mut() {
                annotate_index_range_scans(executor, input, child);
            }
        }
        LogicalPlan::Join { left, right, .. } => {
            let mut children = node.children.iter_mut();
            if let Some(child) = children.next() {
                annotate_index_range_scans(executor, left, child);
            }
            if let Some(child) = children.next() {
                annotate_index_range_scans(executor, right, child);
            }
        }
        _ => {}
    }
}

/// Depth-first search for the first rendered node of the given type.
fn find_node_mut<'n>(
    node: &'n mut crate::sql::explain::PlanNode,
    node_type: &str,
) -> Option<&'n mut crate::sql::explain::PlanNode> {
    if node.node_type == node_type {
        return Some(node);
    }
    node.children
        .iter_mut()
        .find_map(|child| find_node_mut(child, node_type))
}

/// Execute the query for EXPLAIN ANALYZE
fn execute_for_analyze(executor: &Executor, plan: &LogicalPlan) -> (Option<usize>, Option<f64>, Option<String>) {
    let start_time = Instant::now();

    if let Some(storage) = executor.storage() {
        let mut exec = Executor::with_storage(storage);
        if let Some(txn) = executor.transaction() {
            exec = exec.with_transaction(txn);
        }
        if let Some(timeout) = executor.timeout_ctx() {
            exec = exec.with_timeout(Some(timeout.elapsed().as_millis() as u64));
        }

        match exec.execute(plan) {
            Ok(results) => {
                let elapsed_ms = start_time.elapsed().as_secs_f64() * 1000.0;
                (Some(results.len()), Some(elapsed_ms), None)
            }
            Err(e) => {
                let elapsed_ms = start_time.elapsed().as_secs_f64() * 1000.0;
                (None, Some(elapsed_ms), Some(format!("{}", e)))
            }
        }
    } else {
        (None, None, None)
    }
}

/// Format output as JSON
fn format_json_output(
    output: &ExplainOutput,
    storage_features: &[crate::sql::explain_storage::StorageFeatureReport],
    options: &ExplainOptions,
) -> String {
    use serde_json::json;

    let mut result = serde_json::to_value(output).unwrap_or(json!({}));

    // Add storage features if present
    if options.storage && !storage_features.is_empty() {
        if let serde_json::Value::Object(ref mut map) = result {
            map.insert(
                "storage_features".to_string(),
                serde_json::to_value(storage_features).unwrap_or(json!([])),
            );
        }
    }

    // Add options summary
    if let serde_json::Value::Object(ref mut map) = result {
        map.insert(
            "options".to_string(),
            json!({
                "analyze": options.analyze,
                "verbose": options.verbose,
                "format": options.format.name(),
                "costs": options.costs,
                "storage": options.storage,
                "ai": options.ai,
                "why_not": options.why_not,
            }),
        );
    }

    serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string())
}

/// Format output as YAML
fn format_yaml_output(
    output: &ExplainOutput,
    storage_features: &[crate::sql::explain_storage::StorageFeatureReport],
    options: &ExplainOptions,
) -> String {
    // Build a combined structure for YAML output
    let mut yaml_parts = Vec::new();

    // Main explain output
    if let Ok(yaml) = serde_yaml::to_string(output) {
        yaml_parts.push(yaml);
    }

    // Storage features
    if options.storage && !storage_features.is_empty() {
        yaml_parts.push("\n# Storage Features".to_string());
        if let Ok(yaml) = serde_yaml::to_string(storage_features) {
            yaml_parts.push(yaml);
        }
    }

    yaml_parts.join("\n")
}

/// Format output as text (or tree)
fn format_text_output(
    output: &ExplainOutput,
    storage_features: &[crate::sql::explain_storage::StorageFeatureReport],
    options: &ExplainOptions,
    explain_planner: &ExplainPlanner,
) -> String {
    let mut result = String::new();

    // Header
    let header = build_header(options);
    result.push_str(&header);
    result.push_str("\n\n");

    // Use ExplainPlanner's format_output for the main plan
    let plan_output = explain_planner.format_output(output);
    result.push_str(&plan_output);

    // Add execution results if ANALYZE was used
    if options.analyze {
        result.push('\n');
        result.push_str(&format_execution_results(output));
    }

    // Add storage features if requested
    if options.storage && !storage_features.is_empty() {
        result.push_str(&format_storage_features_text(storage_features));
    }

    // Add summary if requested
    if options.summary {
        result.push_str(&format_summary(output, options));
    }

    result
}

/// Build the EXPLAIN header based on options
fn build_header(options: &ExplainOptions) -> String {
    let mut parts = vec!["EXPLAIN"];

    if options.analyze {
        parts.push("ANALYZE");
    }
    if options.verbose {
        parts.push("VERBOSE");
    }
    if options.storage {
        parts.push("STORAGE");
    }
    if options.ai {
        parts.push("AI");
    }
    if options.why_not {
        parts.push("WHY_NOT");
    }
    let format_str;
    if options.format != ExplainFormatOption::Text {
        format_str = format!("FORMAT {}", options.format.name());
        parts.push(&format_str);
    }

    parts.join(" ")
}

/// Format execution results for EXPLAIN ANALYZE
fn format_execution_results(output: &ExplainOutput) -> String {
    let mut result = String::new();

    result.push_str("───────────────────────────────────────────────────────────────────────────────\n");
    result.push_str("Execution Results\n");
    result.push_str("───────────────────────────────────────────────────────────────────────────────\n");

    if let Some(error) = &output.execution_error {
        result.push_str(&format!("  Execution Error: {}\n", error));
    }

    if let Some(time_ms) = output.actual_time_ms {
        result.push_str(&format!("  Execution Time : {:.3} ms\n", time_ms));
    }

    if let Some(rows) = output.actual_rows {
        result.push_str(&format!("  Actual Rows    : {}\n", rows));
    }

    result.push_str(&format!("  Planning Time  : {:.3} ms\n", output.planning_time_ms));
    result.push_str(&format!("  Estimated Rows : {}\n", output.total_rows));
    result.push_str(&format!("  Estimated Cost : {:.2}\n", output.total_cost));

    result
}

/// Format summary section
fn format_summary(output: &ExplainOutput, options: &ExplainOptions) -> String {
    let mut result = String::new();

    result.push_str("\n");
    result.push_str("═══════════════════════════════════════════════════════════════════════════════\n");
    result.push_str("                                  SUMMARY                                     \n");
    result.push_str("═══════════════════════════════════════════════════════════════════════════════\n\n");

    // Comparison of estimates vs actuals
    if options.analyze {
        if let (Some(actual_rows), Some(actual_time)) = (output.actual_rows, output.actual_time_ms) {
            let row_accuracy = if output.total_rows > 0 {
                (actual_rows as f64 / output.total_rows as f64) * 100.0
            } else {
                100.0
            };

            result.push_str(&format!("  Estimate Accuracy:\n"));
            result.push_str(&format!(
                "    Rows: {} actual vs {} estimated ({:.1}%)\n",
                actual_rows, output.total_rows, row_accuracy
            ));
            result.push_str(&format!("    Time: {:.3} ms\n", actual_time));
        }
    }

    // Warnings
    if !output.warnings.is_empty() {
        result.push_str("\n  Warnings:\n");
        for warning in &output.warnings {
            result.push_str(&format!("    - {}\n", warning));
        }
    }

    // Suggestions
    if !output.suggestions.is_empty() {
        result.push_str("\n  Suggestions:\n");
        for suggestion in &output.suggestions {
            result.push_str(&format!("    - {}\n", suggestion));
        }
    }

    result
}

// ═══════════════════════════════════════════════════════════════════════════════
// Legacy format_plan functions (kept for fallback and compatibility)
// ═══════════════════════════════════════════════════════════════════════════════

/// Format a logical plan into human-readable lines (legacy fallback)
#[allow(dead_code)]
fn format_plan(lines: &mut Vec<String>, plan: &LogicalPlan, depth: usize, verbose: bool) {
    let indent = "  ".repeat(depth);
    let arrow = if depth > 0 { "-> " } else { "" };

    match plan {
        LogicalPlan::Scan {
            table_name,
            projection,
            as_of,
            ..
        } => {
            let proj_str = if let Some(proj) = projection {
                format!(" (projection: {:?})", proj)
            } else {
                String::new()
            };
            let as_of_str = if let Some(clause) = as_of {
                format!(" AS OF {:?}", clause)
            } else {
                String::new()
            };
            lines.push(format!(
                "{}{}Seq Scan on {}{}{}",
                indent, arrow, table_name, proj_str, as_of_str
            ));
            if verbose {
                lines.push(format!("{}  Output: all columns", indent));
            }
        }
        LogicalPlan::FilteredScan {
            table_name,
            predicate,
            projection,
            as_of,
            ..
        } => {
            let pred_str = if let Some(pred) = predicate {
                format!(" (filter: {:?})", pred)
            } else {
                String::new()
            };
            let proj_str = if let Some(proj) = projection {
                format!(" (projection: {:?})", proj)
            } else {
                String::new()
            };
            let as_of_str = if let Some(clause) = as_of {
                format!(" AS OF {:?}", clause)
            } else {
                String::new()
            };
            lines.push(format!(
                "{}{}Filtered Scan on {}{}{}{}",
                indent, arrow, table_name, pred_str, proj_str, as_of_str
            ));
        }
        LogicalPlan::Filter { input, predicate } => {
            lines.push(format!("{}{}Filter: {:?}", indent, arrow, predicate));
            format_plan(lines, input, depth + 1, verbose);
        }
        LogicalPlan::Project {
            input,
            aliases,
            distinct,
            ..
        } => {
            let distinct_str = if *distinct { " DISTINCT" } else { "" };
            lines.push(format!(
                "{}{}Project{}: [{}]",
                indent,
                arrow,
                distinct_str,
                aliases.join(", ")
            ));
            format_plan(lines, input, depth + 1, verbose);
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggr_exprs,
            having,
            ..
        } => {
            let group_str = if group_by.is_empty() {
                String::new()
            } else {
                format!(" (GROUP BY {:?})", group_by)
            };
            let having_str = if let Some(h) = having {
                format!(" HAVING {:?}", h)
            } else {
                String::new()
            };
            lines.push(format!(
                "{}{}Aggregate: {:?}{}{}",
                indent, arrow, aggr_exprs, group_str, having_str
            ));
            format_plan(lines, input, depth + 1, verbose);
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            lateral,
        } => {
            let on_str = if let Some(cond) = on {
                format!(" ON {:?}", cond)
            } else {
                String::new()
            };
            let lateral_str = if *lateral { "LATERAL " } else { "" };
            lines.push(format!(
                "{}{}{}Nested Loop {:?} Join{}",
                indent, arrow, lateral_str, join_type, on_str
            ));
            format_plan(lines, left, depth + 1, verbose);
            format_plan(lines, right, depth + 1, verbose);
        }
        LogicalPlan::Sort { input, exprs, asc } => {
            let sort_info: Vec<String> = exprs
                .iter()
                .zip(asc.iter())
                .map(|(e, a)| format!("{:?} {}", e, if *a { "ASC" } else { "DESC" }))
                .collect();
            lines.push(format!("{}{}Sort: [{}]", indent, arrow, sort_info.join(", ")));
            format_plan(lines, input, depth + 1, verbose);
        }
        LogicalPlan::Limit {
            input, limit, offset, ..
        } => {
            let offset_str = if *offset > 0 {
                format!(" OFFSET {}", offset)
            } else {
                String::new()
            };
            lines.push(format!("{}{}Limit: {}{}", indent, arrow, limit, offset_str));
            format_plan(lines, input, depth + 1, verbose);
        }
        LogicalPlan::Explain { input, options } => {
            let mut opts_parts = Vec::new();
            if options.analyze {
                opts_parts.push("ANALYZE");
            }
            if options.verbose {
                opts_parts.push("VERBOSE");
            }
            if options.storage {
                opts_parts.push("STORAGE");
            }
            let opts_str = if opts_parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", opts_parts.join(", "))
            };
            lines.push(format!("{}{}Explain{}", indent, arrow, opts_str));
            format_plan(lines, input, depth + 1, options.verbose);
        }
        // Handle other plan types with basic formatting
        _ => {
            lines.push(format!("{}{}Plan: {:?}", indent, arrow, std::mem::discriminant(plan)));
        }
    }
}
