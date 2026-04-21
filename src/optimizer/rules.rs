//! Optimization rules
//!
//! Standard textbook optimization rules for query plan transformation.
//! Each rule transforms a logical plan to an equivalent but more efficient plan.

#![allow(unreachable_patterns)]

use crate::sql::logical_plan::{LogicalPlan, LogicalExpr, BinaryOperator};
use crate::Result;
use super::cost::{CostEstimator, ColumnStats};
use std::collections::HashSet;

/// Trait for optimization rules
pub trait OptimizationRule: Send + Sync {
    /// Get the name of this rule
    fn name(&self) -> &'static str;

    /// Apply the rule to a logical plan
    /// Returns Some(new_plan) if the rule was applied, None if not applicable
    fn apply(&self, plan: LogicalPlan, cost_estimator: &CostEstimator) -> Result<Option<LogicalPlan>>;

    /// Check if this rule is applicable to a plan
    fn is_applicable(&self, plan: &LogicalPlan) -> bool;
}

// =============================================================================
// Rule 1: Selection Pushdown (Filter Pushdown)
// =============================================================================

/// Pushes filter predicates as close to the data source as possible
///
/// Example transformation:
/// ```text
/// Project(Filter(Scan)) -> Filter(Scan)  // Push filter below projection
/// Filter(Filter(Scan)) -> Filter(Scan)   // Merge multiple filters
/// Join(Filter(A), B) -> Filter(Join(A, B))  // Push filter before join
/// ```
pub struct SelectionPushdownRule;

impl SelectionPushdownRule {
    /// Create a new selection pushdown rule
    pub fn new() -> Self {
        Self
    }

    /// Push filter through projection
    fn push_through_projection(
        &self,
        project: Box<LogicalPlan>,
        filter_pred: LogicalExpr,
    ) -> Result<Option<LogicalPlan>> {
        if let LogicalPlan::Project { input, exprs, aliases, distinct, distinct_on } = *project {
            // Rewrite the filter predicate: replace column references that use
            // projection aliases with the underlying projection expressions.
            // This ensures the predicate is valid against the input schema
            // (e.g., turning `total > 200` into `agg_0 > 200` when pushed
            // below a projection that aliases agg_0 AS total).
            let rewritten_pred = Self::rewrite_predicate_for_pushdown(&filter_pred, &exprs, &aliases);

            // Create filter below projection with rewritten predicate
            let new_filter = LogicalPlan::Filter {
                input,
                predicate: rewritten_pred,
            };

            // Recreate projection on top
            Ok(Some(LogicalPlan::Project {
                input: Box::new(new_filter),
                exprs,
                aliases,
                distinct,
                distinct_on,
            }))
        } else {
            Ok(None)
        }
    }

    /// Rewrite a filter predicate for pushdown below a projection.
    ///
    /// Replaces column references that match projection aliases with
    /// the corresponding projection expressions, so the predicate
    /// is valid against the projection's input schema.
    fn rewrite_predicate_for_pushdown(
        pred: &LogicalExpr,
        proj_exprs: &[LogicalExpr],
        proj_aliases: &[String],
    ) -> LogicalExpr {
        match pred {
            LogicalExpr::Column { table: None, name } => {
                // Check if this column name matches a projection alias
                if let Some(idx) = proj_aliases.iter().position(|a| a == name) {
                    if let Some(expr) = proj_exprs.get(idx) {
                        return expr.clone();
                    }
                }
                pred.clone()
            }
            LogicalExpr::BinaryExpr { left, op, right } => {
                LogicalExpr::BinaryExpr {
                    left: Box::new(Self::rewrite_predicate_for_pushdown(left, proj_exprs, proj_aliases)),
                    op: op.clone(),
                    right: Box::new(Self::rewrite_predicate_for_pushdown(right, proj_exprs, proj_aliases)),
                }
            }
            LogicalExpr::UnaryExpr { op, expr } => {
                LogicalExpr::UnaryExpr {
                    op: *op,
                    expr: Box::new(Self::rewrite_predicate_for_pushdown(expr, proj_exprs, proj_aliases)),
                }
            }
            LogicalExpr::IsNull { expr, is_null } => {
                LogicalExpr::IsNull {
                    expr: Box::new(Self::rewrite_predicate_for_pushdown(expr, proj_exprs, proj_aliases)),
                    is_null: *is_null,
                }
            }
            LogicalExpr::Between { expr, low, high, negated } => {
                LogicalExpr::Between {
                    expr: Box::new(Self::rewrite_predicate_for_pushdown(expr, proj_exprs, proj_aliases)),
                    low: Box::new(Self::rewrite_predicate_for_pushdown(low, proj_exprs, proj_aliases)),
                    high: Box::new(Self::rewrite_predicate_for_pushdown(high, proj_exprs, proj_aliases)),
                    negated: *negated,
                }
            }
            LogicalExpr::InList { expr, list, negated } => {
                LogicalExpr::InList {
                    expr: Box::new(Self::rewrite_predicate_for_pushdown(expr, proj_exprs, proj_aliases)),
                    list: list.iter()
                        .map(|e| Self::rewrite_predicate_for_pushdown(e, proj_exprs, proj_aliases))
                        .collect(),
                    negated: *negated,
                }
            }
            LogicalExpr::Case { expr: case_expr, when_then, else_result } => {
                LogicalExpr::Case {
                    expr: case_expr.as_ref().map(|o| Box::new(Self::rewrite_predicate_for_pushdown(o, proj_exprs, proj_aliases))),
                    when_then: when_then.iter()
                        .map(|(w, t)| (
                            Self::rewrite_predicate_for_pushdown(w, proj_exprs, proj_aliases),
                            Self::rewrite_predicate_for_pushdown(t, proj_exprs, proj_aliases),
                        ))
                        .collect(),
                    else_result: else_result.as_ref().map(|e| Box::new(Self::rewrite_predicate_for_pushdown(e, proj_exprs, proj_aliases))),
                }
            }
            LogicalExpr::ScalarFunction { fun, args } => {
                LogicalExpr::ScalarFunction {
                    fun: fun.clone(),
                    args: args.iter()
                        .map(|a| Self::rewrite_predicate_for_pushdown(a, proj_exprs, proj_aliases))
                        .collect(),
                }
            }
            LogicalExpr::Cast { expr, data_type } => {
                LogicalExpr::Cast {
                    expr: Box::new(Self::rewrite_predicate_for_pushdown(expr, proj_exprs, proj_aliases)),
                    data_type: data_type.clone(),
                }
            }
            // For all other expression types (literals, wildcards, subqueries,
            // table-qualified columns, etc.), return as-is
            _ => pred.clone(),
        }
    }

    /// Merge consecutive filters using AND
    fn merge_filters(
        &self,
        inner_filter: Box<LogicalPlan>,
        outer_pred: LogicalExpr,
    ) -> Result<Option<LogicalPlan>> {
        if let LogicalPlan::Filter { input, predicate: inner_pred } = *inner_filter {
            // Combine predicates with AND
            let combined = LogicalExpr::BinaryExpr {
                left: Box::new(outer_pred),
                op: BinaryOperator::And,
                right: Box::new(inner_pred),
            };

            Ok(Some(LogicalPlan::Filter {
                input,
                predicate: combined,
            }))
        } else {
            Ok(None)
        }
    }

    /// Split AND predicates and push each part down separately
    fn split_and_push_conjuncts(
        &self,
        input: Box<LogicalPlan>,
        predicate: LogicalExpr,
    ) -> Result<Option<LogicalPlan>> {
        let conjuncts = Self::extract_conjuncts(&predicate);

        if conjuncts.len() <= 1 {
            return Ok(None); // Nothing to split
        }

        // Create nested filters for each conjunct
        let mut current = *input;
        for conjunct in conjuncts {
            current = LogicalPlan::Filter {
                input: Box::new(current),
                predicate: conjunct,
            };
        }

        Ok(Some(current))
    }

    /// Push filter predicates through a join.
    ///
    /// Splits AND conjuncts and classifies each by which side of the join it
    /// references. Predicates that only touch the left side push below as
    /// Filter(left), those touching only the right side push below as
    /// Filter(right), and cross-table predicates remain above the join.
    fn push_through_join(
        &self,
        join_node: Box<LogicalPlan>,
        predicate: LogicalExpr,
    ) -> Result<Option<LogicalPlan>> {
        if let LogicalPlan::Join { left, right, join_type, on, lateral } = *join_node {
            // Collect table names/aliases reachable from each side
            let left_tables = Self::collect_table_refs(&left);
            let right_tables = Self::collect_table_refs(&right);

            // Split predicate into AND conjuncts
            let conjuncts = Self::extract_conjuncts(&predicate);

            let mut left_preds = Vec::new();
            let mut right_preds = Vec::new();
            let mut remaining_preds = Vec::new();

            for conjunct in conjuncts {
                let refs = Self::extract_column_table_refs(&conjunct);

                // If no table qualifiers, can't push down safely
                if refs.is_empty() {
                    remaining_preds.push(conjunct);
                    continue;
                }

                let touches_left = refs.iter().any(|r| left_tables.contains(r));
                let touches_right = refs.iter().any(|r| right_tables.contains(r));

                match (touches_left, touches_right) {
                    (true, false) => left_preds.push(conjunct),
                    (false, true) => {
                        // For LEFT/FULL joins, pushing filters to the right side changes semantics
                        if matches!(join_type, crate::sql::JoinType::Inner | crate::sql::JoinType::Right) {
                            right_preds.push(conjunct);
                        } else {
                            remaining_preds.push(conjunct);
                        }
                    }
                    _ => remaining_preds.push(conjunct), // Cross-table or ambiguous
                }
            }

            // Only proceed if we can push at least one predicate down
            if left_preds.is_empty() && right_preds.is_empty() {
                return Ok(None);
            }

            // Wrap left input with pushed-down filters
            let new_left = if left_preds.is_empty() {
                left
            } else {
                let combined = Self::combine_conjuncts(left_preds);
                Box::new(LogicalPlan::Filter {
                    input: left,
                    predicate: combined,
                })
            };

            // Wrap right input with pushed-down filters
            let new_right = if right_preds.is_empty() {
                right
            } else {
                let combined = Self::combine_conjuncts(right_preds);
                Box::new(LogicalPlan::Filter {
                    input: right,
                    predicate: combined,
                })
            };

            // Reconstruct join
            let new_join = LogicalPlan::Join {
                left: new_left,
                right: new_right,
                join_type,
                on,
                lateral,
            };

            // If remaining predicates exist, wrap with Filter
            if remaining_preds.is_empty() {
                Ok(Some(new_join))
            } else {
                let combined = Self::combine_conjuncts(remaining_preds);
                Ok(Some(LogicalPlan::Filter {
                    input: Box::new(new_join),
                    predicate: combined,
                }))
            }
        } else {
            Ok(None)
        }
    }

    /// Collect all table names and aliases reachable from a plan subtree
    fn collect_table_refs(plan: &LogicalPlan) -> HashSet<String> {
        let mut refs = HashSet::new();
        Self::collect_table_refs_inner(plan, &mut refs);
        refs
    }

    fn collect_table_refs_inner(plan: &LogicalPlan, refs: &mut HashSet<String>) {
        match plan {
            LogicalPlan::Scan { table_name, alias, .. } => {
                refs.insert(table_name.clone());
                if let Some(a) = alias {
                    refs.insert(a.clone());
                }
            }
            LogicalPlan::Filter { input, .. } => Self::collect_table_refs_inner(input, refs),
            LogicalPlan::Project { input, .. } => Self::collect_table_refs_inner(input, refs),
            LogicalPlan::Join { left, right, .. } => {
                Self::collect_table_refs_inner(left, refs);
                Self::collect_table_refs_inner(right, refs);
            }
            LogicalPlan::Sort { input, .. } => Self::collect_table_refs_inner(input, refs),
            LogicalPlan::Limit { input, .. } => Self::collect_table_refs_inner(input, refs),
            LogicalPlan::Aggregate { input, .. } => Self::collect_table_refs_inner(input, refs),
            _ => {}
        }
    }

    /// Extract all table references from column expressions in a predicate
    fn extract_column_table_refs(expr: &LogicalExpr) -> HashSet<String> {
        let mut refs = HashSet::new();
        Self::extract_column_table_refs_inner(expr, &mut refs);
        refs
    }

    fn extract_column_table_refs_inner(expr: &LogicalExpr, refs: &mut HashSet<String>) {
        match expr {
            LogicalExpr::Column { table: Some(t), .. } => {
                refs.insert(t.clone());
            }
            LogicalExpr::BinaryExpr { left, right, .. } => {
                Self::extract_column_table_refs_inner(left, refs);
                Self::extract_column_table_refs_inner(right, refs);
            }
            LogicalExpr::UnaryExpr { expr, .. } => {
                Self::extract_column_table_refs_inner(expr, refs);
            }
            LogicalExpr::IsNull { expr, .. } => {
                Self::extract_column_table_refs_inner(expr, refs);
            }
            LogicalExpr::InList { expr, list, .. } => {
                Self::extract_column_table_refs_inner(expr, refs);
                for item in list {
                    Self::extract_column_table_refs_inner(item, refs);
                }
            }
            LogicalExpr::Between { expr, low, high, .. } => {
                Self::extract_column_table_refs_inner(expr, refs);
                Self::extract_column_table_refs_inner(low, refs);
                Self::extract_column_table_refs_inner(high, refs);
            }
            LogicalExpr::Case { expr, when_then, else_result } => {
                if let Some(op) = expr {
                    Self::extract_column_table_refs_inner(op, refs);
                }
                for (w, t) in when_then {
                    Self::extract_column_table_refs_inner(w, refs);
                    Self::extract_column_table_refs_inner(t, refs);
                }
                if let Some(e) = else_result {
                    Self::extract_column_table_refs_inner(e, refs);
                }
            }
            LogicalExpr::ScalarFunction { args, .. } | LogicalExpr::AggregateFunction { args, .. } => {
                for arg in args {
                    Self::extract_column_table_refs_inner(arg, refs);
                }
            }
            _ => {} // Literals, parameters, wildcards, etc.
        }
    }

    /// Combine conjuncts with AND
    fn combine_conjuncts(mut conjuncts: Vec<LogicalExpr>) -> LogicalExpr {
        debug_assert!(!conjuncts.is_empty());
        if conjuncts.len() == 1 {
            return conjuncts.remove(0);
        }
        let mut result = conjuncts.remove(0);
        for conjunct in conjuncts {
            result = LogicalExpr::BinaryExpr {
                left: Box::new(result),
                op: BinaryOperator::And,
                right: Box::new(conjunct),
            };
        }
        result
    }

    /// Extract AND conjuncts from a predicate
    fn extract_conjuncts(expr: &LogicalExpr) -> Vec<LogicalExpr> {
        match expr {
            LogicalExpr::BinaryExpr { left, op: BinaryOperator::And, right } => {
                let mut result = Self::extract_conjuncts(left);
                result.extend(Self::extract_conjuncts(right));
                result
            }
            _ => vec![expr.clone()],
        }
    }
}

impl OptimizationRule for SelectionPushdownRule {
    fn name(&self) -> &'static str {
        "SelectionPushdown"
    }

    fn is_applicable(&self, plan: &LogicalPlan) -> bool {
        matches!(plan, LogicalPlan::Filter { .. })
    }

    fn apply(&self, plan: LogicalPlan, _cost_estimator: &CostEstimator) -> Result<Option<LogicalPlan>> {
        match plan {
            LogicalPlan::Filter { input, predicate } => {
                // Try to push filter through projection
                if matches!(&*input, LogicalPlan::Project { .. }) {
                    return self.push_through_projection(input, predicate);
                }

                // Try to merge with inner filter
                if matches!(&*input, LogicalPlan::Filter { .. }) {
                    return self.merge_filters(input, predicate);
                }

                // Try to push filter through join
                if matches!(&*input, LogicalPlan::Join { .. }) {
                    return self.push_through_join(input, predicate);
                }

                // Try to split AND conjuncts
                self.split_and_push_conjuncts(input, predicate)
            }
            _ => Ok(None),
        }
    }
}

impl Default for SelectionPushdownRule {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Rule 2: Projection Pruning (Column Elimination)
// =============================================================================

/// Removes unused columns from projections to reduce data transfer
///
/// Example transformation:
/// ```text
/// Project(a, b) <- Project(a, b, c) <- Scan
/// Becomes: Project(a, b) <- Scan
/// ```
pub struct ProjectionPruningRule;

impl ProjectionPruningRule {
    /// Create a new projection pruning rule
    pub fn new() -> Self {
        Self
    }

    /// Collect columns used by expressions
    fn collect_used_columns(expr: &LogicalExpr, columns: &mut HashSet<String>) {
        match expr {
            LogicalExpr::Column { name, .. } => {
                columns.insert(name.clone());
            }
            LogicalExpr::BinaryExpr { left, right, .. } => {
                Self::collect_used_columns(left, columns);
                Self::collect_used_columns(right, columns);
            }
            LogicalExpr::UnaryExpr { expr, .. } => {
                Self::collect_used_columns(expr, columns);
            }
            LogicalExpr::AggregateFunction { args, .. } |
            LogicalExpr::ScalarFunction { args, .. } => {
                for arg in args {
                    Self::collect_used_columns(arg, columns);
                }
            }
            LogicalExpr::Case { expr, when_then, else_result } => {
                if let Some(e) = expr {
                    Self::collect_used_columns(e, columns);
                }
                for (when, then) in when_then {
                    Self::collect_used_columns(when, columns);
                    Self::collect_used_columns(then, columns);
                }
                if let Some(e) = else_result {
                    Self::collect_used_columns(e, columns);
                }
            }
            LogicalExpr::Cast { expr, .. } => {
                Self::collect_used_columns(expr, columns);
            }
            LogicalExpr::IsNull { expr, .. } => {
                Self::collect_used_columns(expr, columns);
            }
            LogicalExpr::Between { expr, low, high, .. } => {
                Self::collect_used_columns(expr, columns);
                Self::collect_used_columns(low, columns);
                Self::collect_used_columns(high, columns);
            }
            LogicalExpr::InList { expr, list, .. } => {
                Self::collect_used_columns(expr, columns);
                for item in list {
                    Self::collect_used_columns(item, columns);
                }
            }
            LogicalExpr::InSet { expr, .. } => {
                Self::collect_used_columns(expr, columns);
            }
            LogicalExpr::InSubquery { expr, .. } => {
                // Collect from the main expression; subquery columns are independent
                Self::collect_used_columns(expr, columns);
            }
            LogicalExpr::ScalarSubquery { .. } => {
                // Subquery columns are independent of the outer plan.
            }
            LogicalExpr::Exists { .. } => {
                // EXISTS subquery has no direct column references from outer query
                // (correlated subqueries would need different handling)
            }
            LogicalExpr::NewRow { column } => {
                columns.insert(column.clone());
            }
            LogicalExpr::OldRow { column } => {
                columns.insert(column.clone());
            }
            LogicalExpr::ArraySubscript { array, index } => {
                Self::collect_used_columns(array, columns);
                Self::collect_used_columns(index, columns);
            }
            LogicalExpr::WindowFunction { args, partition_by, order_by, .. } => {
                for arg in args {
                    Self::collect_used_columns(arg, columns);
                }
                for expr in partition_by {
                    Self::collect_used_columns(expr, columns);
                }
                for (expr, _) in order_by {
                    Self::collect_used_columns(expr, columns);
                }
            }
            LogicalExpr::Tuple { items } => {
                for item in items {
                    Self::collect_used_columns(item, columns);
                }
            }
            LogicalExpr::Literal(_) |
            LogicalExpr::Wildcard |
            LogicalExpr::Parameter { .. } => {}
        }
    }

    /// Prune projection to only include used columns
    fn prune_projection(
        &self,
        input: Box<LogicalPlan>,
        exprs: Vec<LogicalExpr>,
        aliases: Vec<String>,
    ) -> Result<Option<LogicalPlan>> {
        // Collect columns actually used
        let mut used_columns = HashSet::new();
        for expr in &exprs {
            Self::collect_used_columns(expr, &mut used_columns);
        }

        // If input is a scan, add projection to it
        if let LogicalPlan::Scan { table_name, alias, schema, projection, as_of } = *input {
            if projection.is_some() {
                // Already has projection, can't optimize further
                return Ok(None);
            }

            // Calculate which column indices to keep
            let mut new_projection = Vec::new();
            for (idx, column) in schema.columns.iter().enumerate() {
                if used_columns.contains(&column.name) {
                    new_projection.push(idx);
                }
            }

            if new_projection.len() < schema.columns.len() {
                // We can prune some columns
                let new_scan = LogicalPlan::Scan {
                    table_name,
                    alias,
                    schema,
                    projection: Some(new_projection),
                    as_of,
                };

                return Ok(Some(LogicalPlan::Project {
                    input: Box::new(new_scan),
                    exprs,
                    aliases,
                    distinct: false,
                    distinct_on: None,
                }));
            }
        }

        Ok(None)
    }
}

impl OptimizationRule for ProjectionPruningRule {
    fn name(&self) -> &'static str {
        "ProjectionPruning"
    }

    fn is_applicable(&self, plan: &LogicalPlan) -> bool {
        matches!(plan, LogicalPlan::Project { .. })
    }

    fn apply(&self, plan: LogicalPlan, _cost_estimator: &CostEstimator) -> Result<Option<LogicalPlan>> {
        match plan {
            LogicalPlan::Project { input, exprs, aliases, distinct: false, distinct_on: None } => {
                self.prune_projection(input, exprs, aliases)
            }
            _ => Ok(None),
        }
    }
}

impl Default for ProjectionPruningRule {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Rule 3: Join Reordering (Put Smallest Tables First)
// =============================================================================

/// Reorders joins to process smallest tables first, reducing intermediate result sizes
///
/// Strategy: Place smaller table on the left (build side) for hash joins
pub struct JoinReorderingRule;

impl JoinReorderingRule {
    /// Create a new join reordering rule
    pub fn new() -> Self {
        Self
    }

    /// Get estimated size of a plan
    fn estimate_size(plan: &LogicalPlan, cost_estimator: &CostEstimator) -> Result<f64> {
        cost_estimator.estimate_cardinality(plan)
    }
}

impl OptimizationRule for JoinReorderingRule {
    fn name(&self) -> &'static str {
        "JoinReordering"
    }

    fn is_applicable(&self, plan: &LogicalPlan) -> bool {
        matches!(plan, LogicalPlan::Join { .. })
    }

    fn apply(&self, plan: LogicalPlan, cost_estimator: &CostEstimator) -> Result<Option<LogicalPlan>> {
        match plan {
            LogicalPlan::Join { left, right, join_type, on, lateral } => {
                // Only reorder inner joins (outer joins are order-dependent)
                // Also don't reorder LATERAL joins (right depends on left)
                if !matches!(join_type, crate::sql::logical_plan::JoinType::Inner) || lateral {
                    return Ok(None);
                }

                let left_size = Self::estimate_size(&left, cost_estimator)?;
                let right_size = Self::estimate_size(&right, cost_estimator)?;

                // If right is smaller, swap
                if right_size < left_size {
                    // Swap left and right
                    Ok(Some(LogicalPlan::Join {
                        left: right,
                        right: left,
                        join_type,
                        on,
                        lateral: false,
                    }))
                } else {
                    Ok(None) // Already in optimal order
                }
            }
            _ => Ok(None),
        }
    }
}

impl Default for JoinReorderingRule {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Rule 4: Index Selection
// =============================================================================

/// Chooses the best index for filter predicates
///
/// Marks scans to use indexes when beneficial predicates exist
pub struct IndexSelectionRule;

impl IndexSelectionRule {
    /// Create a new index selection rule
    pub fn new() -> Self {
        Self
    }

    /// Find filter predicate above scan
    fn find_filter_for_scan(plan: &LogicalPlan) -> Option<&LogicalExpr> {
        match plan {
            LogicalPlan::Filter { predicate, .. } => Some(predicate),
            _ => None,
        }
    }

    /// Check if predicate can use index on column
    fn can_use_index(predicate: &LogicalExpr, column_name: &str) -> bool {
        match predicate {
            LogicalExpr::BinaryExpr { left, op, .. } => {
                // Check if left side is the indexed column
                if let LogicalExpr::Column { name, .. } = left.as_ref() {
                    if name == column_name {
                        // Check if operator is index-compatible
                        return matches!(op,
                            BinaryOperator::Eq |
                            BinaryOperator::Lt |
                            BinaryOperator::LtEq |
                            BinaryOperator::Gt |
                            BinaryOperator::GtEq |
                            BinaryOperator::VectorL2Distance |
                            BinaryOperator::VectorCosineDistance |
                            BinaryOperator::VectorInnerProduct
                        );
                    }
                }
                false
            }
            LogicalExpr::BinaryExpr { op: BinaryOperator::And, left, right } => {
                // Check both sides of AND
                Self::can_use_index(left, column_name) || Self::can_use_index(right, column_name)
            }
            _ => false,
        }
    }

    /// Score index usefulness (higher is better)
    fn score_index(
        _index_type: &str,
        _predicate: &LogicalExpr,
        col_stats: &ColumnStats,
    ) -> f64 {
        // If index exists and has good selectivity, it's useful
        if col_stats.has_index {
            // Estimate selectivity improvement
            let selectivity = col_stats.estimate_selectivity(&BinaryOperator::Eq);
            // Lower selectivity (fewer rows) = higher score
            1.0 - selectivity
        } else {
            0.0
        }
    }
}

impl OptimizationRule for IndexSelectionRule {
    fn name(&self) -> &'static str {
        "IndexSelection"
    }

    fn is_applicable(&self, plan: &LogicalPlan) -> bool {
        // Applicable to Filter -> Scan patterns
        match plan {
            LogicalPlan::Filter { input, .. } => {
                matches!(&**input, LogicalPlan::Scan { .. })
            }
            _ => false,
        }
    }

    fn apply(&self, plan: LogicalPlan, cost_estimator: &CostEstimator) -> Result<Option<LogicalPlan>> {
        match plan {
            LogicalPlan::Filter { input, predicate } => {
                if let LogicalPlan::Scan { table_name, alias, schema, projection, as_of } = *input {
                    // Get table statistics
                    let stats = match cost_estimator.stats().get_table_stats(&table_name) {
                        Some(s) => s,
                        None => return Ok(None), // No stats available
                    };

                    // Find best index for this predicate
                    let mut best_index: Option<(String, f64)> = None;

                    for (col_name, col_stats) in &stats.column_stats {
                        if col_stats.has_index && Self::can_use_index(&predicate, col_name) {
                            let score = Self::score_index(
                                col_stats.index_type.as_ref().map(|s| s.as_str()).unwrap_or(""),
                                &predicate,
                                col_stats,
                            );

                            if let Some((_, current_score)) = best_index {
                                if score > current_score {
                                    best_index = Some((col_name.clone(), score));
                                }
                            } else {
                                best_index = Some((col_name.clone(), score));
                            }
                        }
                    }

                    // If we found a good index, annotate the plan
                    // Note: In a real implementation, we'd add index hints to the scan
                    // For now, we just return None as the plan structure doesn't change
                    // The executor would use the statistics to choose index scans

                    if best_index.is_some() {
                        // Index would be beneficial - in production, we'd add metadata here
                        // For this implementation, the presence in stats is sufficient
                    }

                    // Reconstruct the original plan
                    Ok(Some(LogicalPlan::Filter {
                        input: Box::new(LogicalPlan::Scan {
                            table_name,
                            alias,
                            schema,
                            projection,
                            as_of,
                        }),
                        predicate,
                    }))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }
}

impl Default for IndexSelectionRule {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Rule 5: Constant Folding
// =============================================================================

/// Evaluates constant expressions at planning time
///
/// Example transformations:
/// - `1 + 2` -> `3`
/// - `'hello' || 'world'` -> `'helloworld'`
/// - `NOT FALSE` -> `TRUE`
pub struct ConstantFoldingRule;

impl ConstantFoldingRule {
    /// Create a new constant folding rule
    pub fn new() -> Self {
        Self
    }

    /// Try to fold an expression to a constant
    fn fold_expr(expr: LogicalExpr) -> Result<LogicalExpr> {
        match expr {
            LogicalExpr::BinaryExpr { left, op, right } => {
                // Recursively fold children
                let left = Self::fold_expr(*left)?;
                let right = Self::fold_expr(*right)?;

                // If both sides are literals, try to evaluate
                if let (LogicalExpr::Literal(left_val), LogicalExpr::Literal(right_val)) = (&left, &right) {
                    match op {
                        BinaryOperator::Plus => {
                            if let (crate::Value::Int4(l), crate::Value::Int4(r)) = (left_val, right_val) {
                                return Ok(LogicalExpr::Literal(crate::Value::Int4(l + r)));
                            }
                        }
                        BinaryOperator::Minus => {
                            if let (crate::Value::Int4(l), crate::Value::Int4(r)) = (left_val, right_val) {
                                return Ok(LogicalExpr::Literal(crate::Value::Int4(l - r)));
                            }
                        }
                        BinaryOperator::Multiply => {
                            if let (crate::Value::Int4(l), crate::Value::Int4(r)) = (left_val, right_val) {
                                return Ok(LogicalExpr::Literal(crate::Value::Int4(l * r)));
                            }
                        }
                        BinaryOperator::Divide => {
                            if let (crate::Value::Int4(l), crate::Value::Int4(r)) = (left_val, right_val) {
                                if *r != 0 {
                                    return Ok(LogicalExpr::Literal(crate::Value::Int4(l / r)));
                                }
                            }
                        }
                        BinaryOperator::Eq => {
                            let result = left_val == right_val;
                            return Ok(LogicalExpr::Literal(crate::Value::Boolean(result)));
                        }
                        BinaryOperator::NotEq => {
                            let result = left_val != right_val;
                            return Ok(LogicalExpr::Literal(crate::Value::Boolean(result)));
                        }
                        BinaryOperator::And => {
                            if let (crate::Value::Boolean(l), crate::Value::Boolean(r)) = (left_val, right_val) {
                                return Ok(LogicalExpr::Literal(crate::Value::Boolean(*l && *r)));
                            }
                        }
                        BinaryOperator::Or => {
                            if let (crate::Value::Boolean(l), crate::Value::Boolean(r)) = (left_val, right_val) {
                                return Ok(LogicalExpr::Literal(crate::Value::Boolean(*l || *r)));
                            }
                        }
                        _ => {}
                    }
                }

                // Return simplified expression
                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                })
            }
            LogicalExpr::UnaryExpr { op, expr } => {
                let expr = Self::fold_expr(*expr)?;

                if let LogicalExpr::Literal(val) = &expr {
                    match op {
                        crate::sql::logical_plan::UnaryOperator::Not => {
                            if let crate::Value::Boolean(b) = val {
                                return Ok(LogicalExpr::Literal(crate::Value::Boolean(!b)));
                            }
                        }
                        crate::sql::logical_plan::UnaryOperator::Minus => {
                            if let crate::Value::Int4(i) = val {
                                return Ok(LogicalExpr::Literal(crate::Value::Int4(-i)));
                            }
                        }
                        crate::sql::logical_plan::UnaryOperator::Plus => {
                            return Ok(LogicalExpr::Literal(val.clone()));
                        }
                    }
                }

                Ok(LogicalExpr::UnaryExpr {
                    op,
                    expr: Box::new(expr),
                })
            }
            // For other expressions, return as-is
            other => Ok(other),
        }
    }

    /// Fold expressions in a plan
    fn fold_plan(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        match plan {
            LogicalPlan::Filter { input, predicate } => {
                let folded_predicate = Self::fold_expr(predicate)?;
                Ok(LogicalPlan::Filter {
                    input,
                    predicate: folded_predicate,
                })
            }
            LogicalPlan::Project { input, exprs, aliases, distinct, distinct_on } => {
                let folded_exprs: Result<Vec<_>> = exprs.into_iter()
                    .map(|e| Self::fold_expr(e))
                    .collect();
                Ok(LogicalPlan::Project {
                    input,
                    exprs: folded_exprs?,
                    aliases,
                    distinct,
                    distinct_on,
                })
            }
            other => Ok(other),
        }
    }
}

impl OptimizationRule for ConstantFoldingRule {
    fn name(&self) -> &'static str {
        "ConstantFolding"
    }

    fn is_applicable(&self, plan: &LogicalPlan) -> bool {
        matches!(plan,
            LogicalPlan::Filter { .. } |
            LogicalPlan::Project { .. }
        )
    }

    fn apply(&self, plan: LogicalPlan, _cost_estimator: &CostEstimator) -> Result<Option<LogicalPlan>> {
        let folded = self.fold_plan(plan)?;
        Ok(Some(folded))
    }
}

impl Default for ConstantFoldingRule {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Rule 6: Storage-Level Filter Pushdown
// =============================================================================

/// Pushes filter predicates directly into the storage layer scan operation
///
/// This rule transforms Filter → Scan patterns into a single FilteredScan
/// that can leverage bloom filters, zone maps, and SIMD-accelerated filtering
/// at the storage level.
///
/// Example transformation:
/// ```text
/// Filter(predicate, Scan(table)) -> FilteredScan(table, predicate)
/// ```
///
/// Benefits:
/// - Bloom filters can skip entire tables for equality predicates
/// - Zone maps can skip data blocks for range predicates
/// - SIMD filtering evaluates predicates in vectorized batches
/// - Early termination with LIMIT pushed to storage
pub struct StorageFilterPushdownRule {
    /// Minimum estimated selectivity to push down (0.0 - 1.0)
    /// Lower selectivity = more selective = better for pushdown
    selectivity_threshold: f64,
}

impl StorageFilterPushdownRule {
    /// Create a new storage filter pushdown rule
    pub fn new() -> Self {
        Self {
            selectivity_threshold: 0.5, // Push down predicates with < 50% selectivity
        }
    }

    /// Create with custom selectivity threshold
    pub fn with_threshold(selectivity_threshold: f64) -> Self {
        Self {
            selectivity_threshold: selectivity_threshold.clamp(0.0, 1.0),
        }
    }

    /// Check if a predicate can be pushed to storage level
    fn can_push_predicate(predicate: &LogicalExpr) -> bool {
        match predicate {
            // Simple column comparisons can be pushed
            LogicalExpr::BinaryExpr { left, op, right } => {
                match op {
                    // Equality and comparison operators
                    BinaryOperator::Eq |
                    BinaryOperator::NotEq |
                    BinaryOperator::Lt |
                    BinaryOperator::LtEq |
                    BinaryOperator::Gt |
                    BinaryOperator::GtEq => {
                        // Check if it's column vs literal
                        let is_column_literal = matches!(
                            (left.as_ref(), right.as_ref()),
                            (LogicalExpr::Column { .. }, LogicalExpr::Literal(_)) |
                            (LogicalExpr::Literal(_), LogicalExpr::Column { .. })
                        );
                        is_column_literal
                    }
                    // AND predicates can be pushed if all parts can be pushed
                    BinaryOperator::And => {
                        Self::can_push_predicate(left) && Self::can_push_predicate(right)
                    }
                    // OR predicates are more complex but can still be pushed
                    BinaryOperator::Or => {
                        Self::can_push_predicate(left) && Self::can_push_predicate(right)
                    }
                    // LIKE can be pushed for prefix patterns
                    BinaryOperator::Like => {
                        matches!(
                            (left.as_ref(), right.as_ref()),
                            (LogicalExpr::Column { .. }, LogicalExpr::Literal(_))
                        )
                    }
                    _ => false,
                }
            }
            // IS NULL / IS NOT NULL can be pushed
            LogicalExpr::IsNull { expr, .. } => {
                matches!(expr.as_ref(), LogicalExpr::Column { .. })
            }
            // BETWEEN can be pushed
            LogicalExpr::Between { expr, low, high, .. } => {
                matches!(expr.as_ref(), LogicalExpr::Column { .. }) &&
                matches!(low.as_ref(), LogicalExpr::Literal(_)) &&
                matches!(high.as_ref(), LogicalExpr::Literal(_))
            }
            // IN lists can be pushed
            LogicalExpr::InList { expr, list, .. } => {
                matches!(expr.as_ref(), LogicalExpr::Column { .. }) &&
                list.iter().all(|e| matches!(e, LogicalExpr::Literal(_)))
            }
            _ => false,
        }
    }

    /// Estimate selectivity of a predicate
    fn estimate_selectivity(&self, predicate: &LogicalExpr, cost_estimator: &CostEstimator) -> f64 {
        match predicate {
            LogicalExpr::BinaryExpr { left, op, right } => {
                match op {
                    BinaryOperator::Eq => {
                        // For equality, try to get column stats
                        if let LogicalExpr::Column { name, .. } = left.as_ref() {
                            if let Some(stats) = self.get_column_stats(name, cost_estimator) {
                                return 1.0 / (stats.distinct_count.max(1) as f64);
                            }
                        }
                        0.1 // Default equality selectivity
                    }
                    BinaryOperator::NotEq => 0.9,
                    BinaryOperator::Lt | BinaryOperator::LtEq |
                    BinaryOperator::Gt | BinaryOperator::GtEq => 0.33,
                    BinaryOperator::And => {
                        let left_sel = self.estimate_selectivity(left, cost_estimator);
                        let right_sel = self.estimate_selectivity(right, cost_estimator);
                        left_sel * right_sel
                    }
                    BinaryOperator::Or => {
                        let left_sel = self.estimate_selectivity(left, cost_estimator);
                        let right_sel = self.estimate_selectivity(right, cost_estimator);
                        left_sel + right_sel - (left_sel * right_sel)
                    }
                    BinaryOperator::Like => 0.1,
                    _ => 0.5,
                }
            }
            LogicalExpr::IsNull { .. } => 0.01, // Nulls are usually rare
            LogicalExpr::Between { .. } => 0.25,
            LogicalExpr::InList { list, .. } => {
                (list.len() as f64 * 0.05).min(0.5) // Rough estimate
            }
            _ => 0.5,
        }
    }

    fn get_column_stats(&self, _column_name: &str, _cost_estimator: &CostEstimator) -> Option<ColumnStats> {
        // In a full implementation, this would look up actual column statistics
        None
    }

    /// Split pushable and non-pushable predicates from an AND expression
    fn split_predicates(&self, predicate: &LogicalExpr) -> (Vec<LogicalExpr>, Vec<LogicalExpr>) {
        let mut pushable = Vec::new();
        let mut remaining = Vec::new();

        Self::collect_conjuncts(predicate, &mut |p| {
            if Self::can_push_predicate(p) {
                pushable.push(p.clone());
            } else {
                remaining.push(p.clone());
            }
        });

        (pushable, remaining)
    }

    fn collect_conjuncts<F>(expr: &LogicalExpr, collector: &mut F)
    where
        F: FnMut(&LogicalExpr),
    {
        if let LogicalExpr::BinaryExpr { left, op: BinaryOperator::And, right } = expr {
            Self::collect_conjuncts(left, collector);
            Self::collect_conjuncts(right, collector);
        } else {
            collector(expr);
        }
    }

    /// Combine predicates with AND
    fn combine_predicates(predicates: Vec<LogicalExpr>) -> Option<LogicalExpr> {
        if predicates.is_empty() {
            return None;
        }

        let mut iter = predicates.into_iter();
        let first = iter.next()?;

        Some(iter.fold(first, |acc, p| LogicalExpr::BinaryExpr {
            left: Box::new(acc),
            op: BinaryOperator::And,
            right: Box::new(p),
        }))
    }
}

impl OptimizationRule for StorageFilterPushdownRule {
    fn name(&self) -> &'static str {
        "StorageFilterPushdown"
    }

    fn is_applicable(&self, plan: &LogicalPlan) -> bool {
        // Look for Filter over Scan pattern
        if let LogicalPlan::Filter { input, .. } = plan {
            return matches!(input.as_ref(), LogicalPlan::Scan { .. });
        }
        false
    }

    fn apply(&self, plan: LogicalPlan, cost_estimator: &CostEstimator) -> Result<Option<LogicalPlan>> {
        if let LogicalPlan::Filter { input, predicate } = plan {
            if let LogicalPlan::Scan { table_name, alias, schema, projection, as_of } = *input {
                // Check if predicate can be pushed down
                if !Self::can_push_predicate(&predicate) {
                    // Cannot push - return original
                    return Ok(Some(LogicalPlan::Filter {
                        input: Box::new(LogicalPlan::Scan {
                            table_name,
                            alias,
                            schema,
                            projection,
                            as_of,
                        }),
                        predicate,
                    }));
                }

                // Estimate selectivity
                let selectivity = self.estimate_selectivity(&predicate, cost_estimator);

                // Only push if selectivity is below threshold
                if selectivity <= self.selectivity_threshold {
                    // Split into pushable and remaining predicates
                    let (pushable, remaining) = self.split_predicates(&predicate);

                    // Create FilteredScan with pushable predicates
                    // For now, we annotate the Scan with the predicate for the executor to use
                    // The executor will pass this to the storage layer's filtered scan
                    let filtered_scan = LogicalPlan::FilteredScan {
                        table_name,
                        alias,
                        schema,
                        projection,
                        predicate: Self::combine_predicates(pushable),
                        as_of,
                    };

                    // If there are remaining predicates, wrap in Filter
                    if let Some(remaining_pred) = Self::combine_predicates(remaining) {
                        return Ok(Some(LogicalPlan::Filter {
                            input: Box::new(filtered_scan),
                            predicate: remaining_pred,
                        }));
                    }

                    return Ok(Some(filtered_scan));
                }
            }
        }

        Ok(None)
    }
}

impl Default for StorageFilterPushdownRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Create all standard optimization rules
pub fn create_default_rules() -> Vec<Box<dyn OptimizationRule>> {
    vec![
        Box::new(ConstantFoldingRule::new()),       // Apply first - simplifies expressions
        Box::new(SelectionPushdownRule::new()),     // Push filters early
        Box::new(ProjectionPruningRule::new()),     // Reduce columns early
        Box::new(IndexSelectionRule::new()),        // Choose indexes
        Box::new(JoinReorderingRule::new()),        // Optimize join order
        Box::new(StorageFilterPushdownRule::new()), // Storage-level filtering (last - converts Filter+Scan to FilteredScan)
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::logical_plan::*;
    use crate::{Schema, Column, DataType, Value};
    use crate::optimizer::cost::{StatsCatalog, TableStats};
    use std::sync::Arc;

    fn create_test_schema() -> Arc<Schema> {
        Arc::new(Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: true,
                    source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                },
                Column {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
                },
            ],
        })
    }

    #[test]
    fn test_constant_folding_arithmetic() {
        let rule = ConstantFoldingRule::new();

        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Literal(Value::Int4(10))),
            op: BinaryOperator::Plus,
            right: Box::new(LogicalExpr::Literal(Value::Int4(5))),
        };

        let folded = ConstantFoldingRule::fold_expr(expr).unwrap();

        assert!(matches!(folded, LogicalExpr::Literal(Value::Int4(15))));
    }

    #[test]
    fn test_constant_folding_boolean() {
        let _rule = ConstantFoldingRule::new();

        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Literal(Value::Boolean(true))),
            op: BinaryOperator::And,
            right: Box::new(LogicalExpr::Literal(Value::Boolean(false))),
        };

        let folded = ConstantFoldingRule::fold_expr(expr).unwrap();

        assert!(matches!(folded, LogicalExpr::Literal(Value::Boolean(false))));
    }

    #[test]
    fn test_selection_pushdown_merge() {
        let rule = SelectionPushdownRule::new();
        let stats_catalog = StatsCatalog::new();
        let estimator = CostEstimator::new(stats_catalog);

        let schema = create_test_schema();

        // Create Filter(Filter(Scan))
        let scan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let inner_filter = LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
                op: BinaryOperator::Gt,
                right: Box::new(LogicalExpr::Literal(Value::Int4(0))),
            },
        };

        let outer_filter = LogicalPlan::Filter {
            input: Box::new(inner_filter),
            predicate: LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
                op: BinaryOperator::Lt,
                right: Box::new(LogicalExpr::Literal(Value::Int4(100))),
            },
        };

        let result = rule.apply(outer_filter, &estimator).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_projection_pruning_columns() {
        let rule = ProjectionPruningRule::new();

        let mut used = HashSet::new();
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string()  }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::Int4(1))),
        };

        ProjectionPruningRule::collect_used_columns(&expr, &mut used);

        assert!(used.contains("id"));
        assert_eq!(used.len(), 1);
    }

    #[test]
    fn test_join_reordering() {
        let mut stats_catalog = StatsCatalog::new();

        // Small table (100 rows)
        stats_catalog.add_table_stats(
            TableStats::new("small".to_string())
                .with_row_count(100)
                .with_avg_row_size(100)
        );

        // Large table (10000 rows)
        stats_catalog.add_table_stats(
            TableStats::new("large".to_string())
                .with_row_count(10000)
                .with_avg_row_size(100)
        );

        let estimator = CostEstimator::new(stats_catalog);
        let rule = JoinReorderingRule::new();

        let schema = create_test_schema();

        // Join with large table on left
        let large_scan = LogicalPlan::Scan {
            table_name: "large".to_string(),
            alias: None,
            schema: schema.clone(),
            projection: None,
            as_of: None,
        };

        let small_scan = LogicalPlan::Scan {
            table_name: "small".to_string(),
            alias: None,
            schema,
            projection: None,
            as_of: None,
        };

        let join = LogicalPlan::Join {
            left: Box::new(large_scan),
            right: Box::new(small_scan),
            join_type: JoinType::Inner,
            on: None,
            lateral: false,
        };

        let result = rule.apply(join, &estimator).unwrap();

        // Should swap to put small table first
        assert!(result.is_some());

        if let Some(LogicalPlan::Join { left, .. }) = result {
            if let LogicalPlan::Scan { table_name, .. } = &*left {
                assert_eq!(table_name, "small");
            }
        }
    }
}
