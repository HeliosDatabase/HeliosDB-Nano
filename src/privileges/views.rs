//! Which scans of a plan came out of a view body.
//!
//! The planner expands a view in place: after planning, a query on a view is
//! a tree of scans of the view's base tables and nothing says a view was ever
//! involved. PostgreSQL checks those base tables against the view's OWNER and
//! checks the caller only against the view, so the gate has to tell the two
//! kinds of scan apart, and it has to be able to do so again later, when a
//! cached plan is checked for another role.
//!
//! A scan node carries its schema as an `Arc`, minted by the planner for that
//! one scan and shared by every clone of the plan. This module remembers, for
//! each such `Arc` that was planned inside a view body, the chain of views it
//! was planned through (innermost first). The registry holds weak references:
//! an entry lives exactly as long as some plan still holds the scan, and its
//! address cannot be reused while the entry exists.
//!
//! A scan that loses its mark (a plan rebuilt from serialized form) is checked
//! against the caller, which can only refuse more.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use crate::sql::{LogicalExpr, LogicalPlan};
use crate::Schema;

type Registry = parking_lot::Mutex<HashMap<usize, (Weak<Schema>, Vec<String>)>>;

fn registry() -> &'static Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Entries are pruned of dead scans when the registry reaches this size.
const PRUNE_AT: usize = 4096;

/// Record that every scan in `body` was planned while expanding `view`.
pub(crate) fn mark_expansion(view: &str, body: &LogicalPlan) {
    let mut schemas: Vec<Arc<Schema>> = Vec::new();
    collect_scan_schemas(body, &mut schemas);
    if schemas.is_empty() {
        return;
    }
    let mut entries = registry().lock();
    if entries.len() >= PRUNE_AT {
        entries.retain(|_, (weak, _)| weak.strong_count() > 0);
    }
    for schema in schemas {
        let key = Arc::as_ptr(&schema) as usize;
        let entry = entries
            .entry(key)
            .or_insert_with(|| (Arc::downgrade(&schema), Vec::new()));
        if entry.0.strong_count() == 0 {
            // A dead entry at a reused address cannot exist (the weak pins the
            // allocation), but never extend a chain that is not this scan's.
            *entry = (Arc::downgrade(&schema), Vec::new());
        }
        entry.1.push(view.to_string());
    }
}

/// The views a scan was planned through, innermost first; `None` for a scan
/// the statement wrote itself.
pub(crate) fn chain_of(schema: &Arc<Schema>) -> Option<Vec<String>> {
    let key = Arc::as_ptr(schema) as usize;
    let entries = registry().lock();
    let (weak, chain) = entries.get(&key)?;
    if !std::ptr::eq(weak.as_ptr(), Arc::as_ptr(schema)) || chain.is_empty() {
        return None;
    }
    Some(chain.clone())
}

fn collect_scan_schemas(plan: &LogicalPlan, out: &mut Vec<Arc<Schema>>) {
    for_each_node(plan, &mut |node| match node {
        LogicalPlan::Scan { schema, .. } | LogicalPlan::FilteredScan { schema, .. } => out.push(Arc::clone(schema)),
        _ => {}
    });
}

/// Visit every plan node of a query, including the subqueries inside its
/// expressions.
pub(crate) fn for_each_node(plan: &LogicalPlan, visit: &mut dyn FnMut(&LogicalPlan)) {
    visit(plan);
    let mut exprs: Vec<&LogicalExpr> = Vec::new();
    let mut children: Vec<&LogicalPlan> = Vec::new();
    match plan {
        LogicalPlan::FilteredScan { predicate, .. } => exprs.extend(predicate.iter()),
        LogicalPlan::Filter { input, predicate } => {
            children.push(input);
            exprs.push(predicate);
        }
        LogicalPlan::Project {
            input,
            exprs: projected,
            distinct_on,
            ..
        } => {
            children.push(input);
            exprs.extend(projected.iter());
            exprs.extend(distinct_on.iter().flatten());
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggr_exprs,
            having,
        } => {
            children.push(input);
            exprs.extend(group_by.iter());
            exprs.extend(aggr_exprs.iter());
            exprs.extend(having.iter());
        }
        LogicalPlan::Join { left, right, on, .. } => {
            children.push(left);
            children.push(right);
            exprs.extend(on.iter());
        }
        LogicalPlan::Sort {
            input, exprs: keys, ..
        } => {
            children.push(input);
            exprs.extend(keys.iter());
        }
        LogicalPlan::Limit { input, .. } => children.push(input),
        LogicalPlan::Union { left, right, .. }
        | LogicalPlan::Intersect { left, right, .. }
        | LogicalPlan::Except { left, right, .. } => {
            children.push(left);
            children.push(right);
        }
        LogicalPlan::With { ctes, query, .. } => {
            for (_, cte, _) in ctes {
                children.push(cte);
            }
            children.push(query);
        }
        LogicalPlan::TableFunction { args, .. } | LogicalPlan::SystemView { args, .. } => exprs.extend(args.iter()),
        _ => {}
    }
    for expr in exprs {
        for_each_subquery(expr, &mut |subquery| for_each_node(subquery, visit));
    }
    for child in children {
        for_each_node(child, visit);
    }
}

/// Visit the subquery plans directly inside an expression.
pub(crate) fn for_each_subquery(expr: &LogicalExpr, visit: &mut dyn FnMut(&LogicalPlan)) {
    match expr {
        LogicalExpr::Column { .. }
        | LogicalExpr::Literal(_)
        | LogicalExpr::DefaultValue
        | LogicalExpr::Wildcard
        | LogicalExpr::Parameter { .. }
        | LogicalExpr::NewRow { .. }
        | LogicalExpr::OldRow { .. }
        | LogicalExpr::BoundColumn { .. } => {}
        LogicalExpr::BinaryExpr { left, right, .. } => {
            for_each_subquery(left, visit);
            for_each_subquery(right, visit);
        }
        LogicalExpr::UnaryExpr { expr, .. } | LogicalExpr::Cast { expr, .. } | LogicalExpr::IsNull { expr, .. } => {
            for_each_subquery(expr, visit);
        }
        LogicalExpr::AggregateFunction { args, .. } | LogicalExpr::ScalarFunction { args, .. } => {
            for arg in args {
                for_each_subquery(arg, visit);
            }
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            if let Some(expr) = expr {
                for_each_subquery(expr, visit);
            }
            for (when, then) in when_then {
                for_each_subquery(when, visit);
                for_each_subquery(then, visit);
            }
            if let Some(else_result) = else_result {
                for_each_subquery(else_result, visit);
            }
        }
        LogicalExpr::Between { expr, low, high, .. } => {
            for_each_subquery(expr, visit);
            for_each_subquery(low, visit);
            for_each_subquery(high, visit);
        }
        LogicalExpr::InList { expr, list, .. } => {
            for_each_subquery(expr, visit);
            for item in list {
                for_each_subquery(item, visit);
            }
        }
        LogicalExpr::InSet { expr, .. } => for_each_subquery(expr, visit),
        LogicalExpr::ScalarSubquery { subquery } | LogicalExpr::Exists { subquery, .. } => visit(subquery),
        LogicalExpr::InSubquery { expr, subquery, .. } => {
            for_each_subquery(expr, visit);
            visit(subquery);
        }
        LogicalExpr::ArraySubscript { array, index } => {
            for_each_subquery(array, visit);
            for_each_subquery(index, visit);
        }
        LogicalExpr::Tuple { items } => {
            for item in items {
                for_each_subquery(item, visit);
            }
        }
        LogicalExpr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                for_each_subquery(arg, visit);
            }
            for expr in partition_by {
                for_each_subquery(expr, visit);
            }
            for (expr, _) in order_by {
                for_each_subquery(expr, visit);
            }
        }
    }
}
