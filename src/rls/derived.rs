//! Policies and materialized views.
//!
//! A materialized view stores rows computed from every row of its base tables,
//! so a policy of a base table can no longer filter the scan when the view is
//! read. It can still be applied to the stored rows, but only when doing so is
//! the same as filtering the base rows first. [`prove`] decides that for one
//! base table and the columns its policy reads:
//!
//! * every column comes through to the output unrenamed and unchanged, through
//!   every projection of the defining query;
//! * wherever the query aggregates, the columns are grouping keys (so each
//!   output row is made of base rows the policy treats alike), and it never
//!   aggregates with no grouping at all (an empty group still makes a row);
//! * nothing mixes rows of different policy values into one output row or
//!   drops a row because of other rows: no `LIMIT`, no `DISTINCT ON`, no window
//!   over other partitions, no set operation other than a `UNION ALL` whose
//!   branches each satisfy the rule, no outer join that can null the table's
//!   side, no second read of the table, no subquery over it.
//!
//! When the proof fails the answer is a [`Refusal`] saying why; the caller
//! turns it into an error. It is never an empty result and never the
//! unfiltered rows.

use crate::sql::logical_plan::JoinType;
use crate::sql::{LogicalExpr, LogicalPlan};

/// Why a policy cannot be applied to a materialized view's stored rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The column is not in the view's output.
    ColumnNotInOutput { column: String },
    /// The column is in the output under another name.
    ColumnRenamed { column: String, output: String },
    /// The output holds a value computed from the column, not the column.
    ColumnComputed { column: String },
    /// The query aggregates and the column is not a grouping key.
    NotGroupingKey { column: String },
    /// The query aggregates with no `GROUP BY`.
    UngroupedAggregate,
    /// Two output columns have the column's name.
    AmbiguousColumn { column: String },
    /// A window function whose partitions are not split by the column.
    WindowAcrossRows { column: Option<String> },
    /// `DISTINCT ON`.
    DistinctOn,
    /// `LIMIT` / `OFFSET`.
    Limit,
    /// `UNION` (without `ALL`), `INTERSECT` or `EXCEPT`.
    SetOperation { operation: &'static str },
    /// A `UNION ALL` branch that does not read the table.
    UnionBranchWithoutTable,
    /// An outer join that can produce rows with the table's side NULL.
    OuterJoin,
    /// The query reads the table more than once.
    TableReadTwice,
    /// A subquery in an expression reads the table.
    SubqueryOverTable,
    /// The policy's own subquery refers to the row being checked.
    CorrelatedPolicy,
    /// A query shape this analysis does not cover.
    Unsupported { construct: &'static str },
}

impl Refusal {
    /// The column the refusal is about, when it is about one.
    pub fn column(&self) -> Option<&str> {
        match self {
            Refusal::ColumnNotInOutput { column }
            | Refusal::ColumnRenamed { column, .. }
            | Refusal::ColumnComputed { column }
            | Refusal::NotGroupingKey { column }
            | Refusal::AmbiguousColumn { column } => Some(column),
            Refusal::WindowAcrossRows { column } => column.as_deref(),
            _ => None,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::ColumnNotInOutput { column } => {
                write!(f, "column \"{column}\" is not preserved: it is not in the view's output")
            }
            Refusal::ColumnRenamed { column, output } => write!(
                f,
                "column \"{column}\" is not preserved: the view's output has it under the name \"{output}\""
            ),
            Refusal::ColumnComputed { column } => write!(
                f,
                "column \"{column}\" is not preserved: the view's output holds a value computed from it"
            ),
            Refusal::NotGroupingKey { column } => write!(
                f,
                "column \"{column}\" is not preserved: the view aggregates and it is not a grouping key"
            ),
            Refusal::UngroupedAggregate => {
                write!(f, "the view aggregates all rows into one (no GROUP BY)")
            }
            Refusal::AmbiguousColumn { column } => write!(
                f,
                "column \"{column}\" is not preserved: more than one column of the view's query has that name"
            ),
            Refusal::WindowAcrossRows { column: Some(column) } => write!(
                f,
                "column \"{column}\" is not preserved: a window function is not partitioned by it"
            ),
            Refusal::WindowAcrossRows { column: None } => {
                write!(f, "a window function reads across the rows the policy tells apart")
            }
            Refusal::DistinctOn => write!(f, "the view uses DISTINCT ON, which keeps one row of several"),
            Refusal::Limit => write!(f, "the view uses LIMIT or OFFSET, which keeps some rows of all"),
            Refusal::SetOperation { operation } => write!(f, "the view uses {operation}"),
            Refusal::UnionBranchWithoutTable => {
                write!(f, "a UNION ALL branch of the view does not read the table")
            }
            Refusal::OuterJoin => write!(f, "an outer join of the view can return rows without the table's row"),
            Refusal::TableReadTwice => write!(f, "the view reads the table more than once"),
            Refusal::SubqueryOverTable => write!(f, "a subquery of the view reads the table"),
            Refusal::CorrelatedPolicy => {
                write!(f, "the policy has a subquery that refers to the row being checked")
            }
            Refusal::Unsupported { construct } => write!(f, "the view uses {construct}"),
        }
    }
}

/// The expressions directly under `expr`, and the queries embedded in it.
fn children(expr: &LogicalExpr) -> (Vec<&LogicalExpr>, Vec<&LogicalPlan>) {
    use LogicalExpr as E;
    let mut exprs: Vec<&LogicalExpr> = Vec::new();
    let mut plans: Vec<&LogicalPlan> = Vec::new();
    match expr {
        E::ScalarSubquery { subquery } | E::Exists { subquery, .. } => plans.push(subquery),
        E::InSubquery { expr, subquery, .. } => {
            exprs.push(expr);
            plans.push(subquery);
        }
        E::BinaryExpr { left, right, .. } => exprs.extend([&**left, &**right]),
        E::UnaryExpr { expr, .. } | E::Cast { expr, .. } | E::IsNull { expr, .. } | E::InSet { expr, .. } => {
            exprs.push(expr);
        }
        E::AggregateFunction { args, .. } | E::ScalarFunction { args, .. } => exprs.extend(args),
        E::Case {
            expr,
            when_then,
            else_result,
        } => {
            exprs.extend(expr.as_deref());
            for (when, then) in when_then {
                exprs.push(when);
                exprs.push(then);
            }
            exprs.extend(else_result.as_deref());
        }
        E::Between { expr, low, high, .. } => exprs.extend([&**expr, &**low, &**high]),
        E::InList { expr, list, .. } => {
            exprs.push(expr);
            exprs.extend(list);
        }
        E::ArraySubscript { array, index } => exprs.extend([&**array, &**index]),
        E::Tuple { items } => exprs.extend(items),
        E::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            exprs.extend(args);
            exprs.extend(partition_by);
            exprs.extend(order_by.iter().map(|(e, _)| e));
        }
        E::Column { .. }
        | E::Literal(_)
        | E::DefaultValue
        | E::Wildcard
        | E::Parameter { .. }
        | E::NewRow { .. }
        | E::OldRow { .. }
        | E::BoundColumn { .. } => {}
    }
    (exprs, plans)
}

/// `expr` and every expression under it outside a subquery, and the queries
/// embedded in them.
fn flatten<'a>(expr: &'a LogicalExpr, exprs: &mut Vec<&'a LogicalExpr>, plans: &mut Vec<&'a LogicalPlan>) {
    exprs.push(expr);
    let (kids, subqueries) = children(expr);
    plans.extend(subqueries);
    for kid in kids {
        flatten(kid, exprs, plans);
    }
}

/// The columns of the row being checked that a policy expression reads: its
/// column references outside any subquery.
pub fn policy_columns(expr: &LogicalExpr) -> Vec<String> {
    let (mut exprs, mut plans) = (Vec::new(), Vec::new());
    flatten(expr, &mut exprs, &mut plans);
    let mut columns: Vec<String> = Vec::new();
    for e in exprs {
        if let LogicalExpr::Column { name, .. } = e {
            if !columns.contains(name) {
                columns.push(name.clone());
            }
        }
    }
    columns
}

/// The inputs and the expressions of a query node; `None` for a node that is
/// not part of a query.
fn parts(plan: &LogicalPlan) -> Option<(Vec<&LogicalPlan>, Vec<&LogicalExpr>)> {
    use LogicalPlan as P;
    Some(match plan {
        P::Scan { .. } | P::DualScan | P::SystemView { .. } => (vec![], vec![]),
        P::FilteredScan { predicate, .. } => (vec![], predicate.iter().collect()),
        P::Filter { input, predicate } => (vec![&**input], vec![predicate]),
        P::Project {
            input,
            exprs,
            distinct_on,
            ..
        } => (
            vec![&**input],
            exprs.iter().chain(distinct_on.iter().flatten()).collect(),
        ),
        P::Aggregate {
            input,
            group_by,
            aggr_exprs,
            having,
        } => (
            vec![&**input],
            group_by.iter().chain(aggr_exprs).chain(having).collect(),
        ),
        P::Join { left, right, on, .. } => (vec![&**left, &**right], on.iter().collect()),
        P::Sort { input, exprs, .. } => (vec![&**input], exprs.iter().collect()),
        P::Limit { input, .. } | P::Explain { input, .. } => (vec![&**input], vec![]),
        P::Union { left, right, .. } | P::Intersect { left, right, .. } | P::Except { left, right, .. } => {
            (vec![&**left, &**right], vec![])
        }
        P::With { ctes, query, .. } => (
            ctes.iter().map(|(_, cte, _)| &**cte).chain([&**query]).collect(),
            vec![],
        ),
        P::TableFunction { args, .. } => (vec![], args.iter().collect()),
        _ => return None,
    })
}

/// Every table a query reads, each once, in the order met. Tables read by
/// subqueries in expressions are included.
pub fn scanned_tables(plan: &LogicalPlan) -> Vec<String> {
    let mut tables = Vec::new();
    collect_tables(plan, &mut tables);
    tables
}

fn collect_tables(plan: &LogicalPlan, tables: &mut Vec<String>) {
    if let LogicalPlan::Scan { table_name, .. } | LogicalPlan::FilteredScan { table_name, .. } = plan {
        if !tables.contains(table_name) {
            tables.push(table_name.clone());
        }
    }
    let Some((inputs, exprs)) = parts(plan) else { return };
    for input in inputs {
        collect_tables(input, tables);
    }
    for expr in exprs {
        let (mut flat, mut subqueries) = (Vec::new(), Vec::new());
        flatten(expr, &mut flat, &mut subqueries);
        for subquery in subqueries {
            collect_tables(subquery, tables);
        }
    }
}

fn reads(plan: &LogicalPlan, table: &str) -> bool {
    scanned_tables(plan).iter().any(|t| t == table)
}

/// A subquery in one of `exprs` reads `table`.
fn subquery_reads(exprs: &[&LogicalExpr], table: &str) -> bool {
    exprs.iter().any(|expr| {
        let (mut flat, mut subqueries) = (Vec::new(), Vec::new());
        flatten(expr, &mut flat, &mut subqueries);
        subqueries.iter().any(|subquery| reads(subquery, table))
    })
}

fn is_column(expr: &LogicalExpr, column: &str) -> bool {
    matches!(expr, LogicalExpr::Column { name, .. } if name == column)
}

fn mentions(expr: &LogicalExpr, column: &str) -> bool {
    let (mut flat, mut subqueries) = (Vec::new(), Vec::new());
    flatten(expr, &mut flat, &mut subqueries);
    flat.iter().any(|e| is_column(e, column))
}

/// Is doing the policy's filtering on the output of `plan` the same as doing
/// it on the rows of `table` first? `columns` are the columns of `table` the
/// policy reads.
pub fn prove(plan: &LogicalPlan, table: &str, columns: &[String]) -> Result<(), Refusal> {
    walk(plan, table, columns).map(drop)
}

/// `Ok(true)`: `plan` reads `table`, each of `columns` is in its output under
/// its own name holding the value of the contributing rows of `table`, and no
/// output row mixes or depends on rows of `table` with other values of them.
/// `Ok(false)`: `plan` does not read `table`.
fn walk(plan: &LogicalPlan, table: &str, columns: &[String]) -> Result<bool, Refusal> {
    use LogicalPlan as P;
    // A subquery over the table, anywhere in this node's expressions, makes
    // an output row depend on rows the policy would have removed.
    if let Some((_, exprs)) = parts(plan) {
        if subquery_reads(&exprs, table) {
            return Err(Refusal::SubqueryOverTable);
        }
    }
    match plan {
        P::Scan {
            table_name,
            schema,
            projection,
            ..
        }
        | P::FilteredScan {
            table_name,
            schema,
            projection,
            ..
        } => {
            if table_name != table {
                return Ok(false);
            }
            for column in columns {
                let kept = match (schema.get_column_index(column), projection) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(index), Some(projected)) => projected.contains(&index),
                };
                if !kept {
                    return Err(Refusal::ColumnNotInOutput { column: column.clone() });
                }
            }
            Ok(true)
        }
        P::DualScan | P::SystemView { .. } | P::TableFunction { .. } => Ok(false),
        P::Filter { input, .. } | P::Sort { input, .. } | P::Explain { input, .. } => walk(input, table, columns),
        P::Limit { input, .. } => {
            if walk(input, table, columns)? {
                return Err(Refusal::Limit);
            }
            Ok(false)
        }
        P::Project {
            input,
            exprs,
            aliases,
            distinct_on,
            ..
        } => {
            if !walk(input, table, columns)? {
                return Ok(false);
            }
            if distinct_on.is_some() {
                return Err(Refusal::DistinctOn);
            }
            let (mut flat, mut subqueries) = (Vec::new(), Vec::new());
            for expr in exprs {
                flatten(expr, &mut flat, &mut subqueries);
            }
            for expr in &flat {
                match expr {
                    LogicalExpr::WindowFunction { partition_by, .. } => {
                        if let Some(column) = columns.iter().find(|c| !partition_by.iter().any(|p| is_column(p, c))) {
                            return Err(Refusal::WindowAcrossRows {
                                column: Some(column.clone()),
                            });
                        }
                    }
                    // An aggregate with no Aggregate node under it: the grouping
                    // is not visible here.
                    LogicalExpr::AggregateFunction { .. } if !matches!(**input, P::Aggregate { .. }) => {
                        return Err(Refusal::Unsupported {
                            construct: "an aggregate this analysis cannot place",
                        });
                    }
                    _ => {}
                }
            }
            // `SELECT *` passes every input column through.
            let passes_all = exprs.iter().any(|e| matches!(e, LogicalExpr::Wildcard));
            let name_of = |index: usize, expr: &LogicalExpr| -> Option<String> {
                match aliases.get(index) {
                    Some(alias) if !alias.is_empty() => Some(alias.clone()),
                    _ => match expr {
                        LogicalExpr::Column { name, .. } => Some(name.clone()),
                        _ => None,
                    },
                }
            };
            for column in columns {
                let named: Vec<&LogicalExpr> = exprs
                    .iter()
                    .enumerate()
                    .filter(|(i, e)| name_of(*i, e).as_deref() == Some(column.as_str()))
                    .map(|(_, e)| e)
                    .collect();
                match named.as_slice() {
                    [only] if is_column(only, column) => {}
                    [] if passes_all => {}
                    [] => {
                        let renamed = exprs
                            .iter()
                            .enumerate()
                            .find(|(_, e)| is_column(e, column))
                            .and_then(|(i, e)| name_of(i, e));
                        return Err(match renamed {
                            Some(output) => Refusal::ColumnRenamed {
                                column: column.clone(),
                                output,
                            },
                            None if exprs.iter().any(|e| mentions(e, column)) => {
                                Refusal::ColumnComputed { column: column.clone() }
                            }
                            None => Refusal::ColumnNotInOutput { column: column.clone() },
                        });
                    }
                    [_] => return Err(Refusal::ColumnComputed { column: column.clone() }),
                    _ => return Err(Refusal::AmbiguousColumn { column: column.clone() }),
                }
            }
            Ok(true)
        }
        P::Aggregate { input, group_by, .. } => {
            if !walk(input, table, columns)? {
                return Ok(false);
            }
            if group_by.is_empty() {
                return Err(Refusal::UngroupedAggregate);
            }
            if let Some(column) = columns.iter().find(|c| !group_by.iter().any(|g| is_column(g, c))) {
                return Err(Refusal::NotGroupingKey { column: column.clone() });
            }
            Ok(true)
        }
        P::Join {
            left,
            right,
            join_type,
            ..
        } => {
            let on_left = walk(left, table, columns)?;
            let on_right = walk(right, table, columns)?;
            let other = match (on_left, on_right) {
                (false, false) => return Ok(false),
                (true, true) => return Err(Refusal::TableReadTwice),
                (true, false) => right,
                (false, true) => left,
            };
            let nulls_table = match join_type {
                JoinType::Inner | JoinType::Cross => false,
                JoinType::Left => on_right,
                JoinType::Right => on_left,
                JoinType::Full => true,
            };
            if nulls_table {
                return Err(Refusal::OuterJoin);
            }
            let other_schema = other.schema();
            if let Some(column) = columns.iter().find(|c| other_schema.get_column_index(c).is_some()) {
                return Err(Refusal::AmbiguousColumn { column: column.clone() });
            }
            Ok(true)
        }
        P::Union { left, right, all } => {
            let on_left = walk(left, table, columns)?;
            let on_right = walk(right, table, columns)?;
            match (on_left, on_right, *all) {
                (false, false, _) => Ok(false),
                (_, _, false) => Err(Refusal::SetOperation { operation: "UNION" }),
                (true, true, true) => Ok(true),
                _ => Err(Refusal::UnionBranchWithoutTable),
            }
        }
        P::Intersect { left, right, .. } => {
            if reads(left, table) || reads(right, table) {
                return Err(Refusal::SetOperation { operation: "INTERSECT" });
            }
            Ok(false)
        }
        P::Except { left, right, .. } => {
            if reads(left, table) || reads(right, table) {
                return Err(Refusal::SetOperation { operation: "EXCEPT" });
            }
            Ok(false)
        }
        P::With { .. } => {
            if reads(plan, table) {
                return Err(Refusal::Unsupported {
                    construct: "a common table expression over the table",
                });
            }
            Ok(false)
        }
        // Not a query node. A stored view definition never holds one; refuse
        // rather than guess.
        _ => Err(Refusal::Unsupported {
            construct: "a statement this analysis does not cover",
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{Column, DataType, Schema, Value};
    use std::sync::Arc;

    fn scan(table: &str, columns: &[&str]) -> LogicalPlan {
        LogicalPlan::Scan {
            table_name: table.to_string(),
            alias: None,
            schema: Arc::new(Schema {
                columns: columns.iter().map(|c| Column::new(*c, DataType::Text)).collect(),
            }),
            projection: None,
            as_of: None,
        }
    }

    fn col(name: &str) -> LogicalExpr {
        LogicalExpr::Column {
            table: None,
            name: name.to_string(),
        }
    }

    fn project(input: LogicalPlan, outputs: &[(LogicalExpr, &str)]) -> LogicalPlan {
        LogicalPlan::Project {
            input: Box::new(input),
            exprs: outputs.iter().map(|(e, _)| e.clone()).collect(),
            aliases: outputs.iter().map(|(_, a)| (*a).to_string()).collect(),
            distinct: false,
            distinct_on: None,
            source_alias: None,
        }
    }

    fn owner() -> Vec<String> {
        vec!["owner".to_string()]
    }

    #[test]
    fn a_plain_projection_that_keeps_the_column_is_accepted() {
        let plan = project(
            scan("docs", &["id", "owner"]),
            &[(col("id"), "id"), (col("owner"), "owner")],
        );
        assert_eq!(prove(&plan, "docs", &owner()), Ok(()));
    }

    #[test]
    fn a_dropped_renamed_or_computed_column_is_refused() {
        let dropped = project(scan("docs", &["id", "owner"]), &[(col("id"), "id")]);
        assert_eq!(
            prove(&dropped, "docs", &owner()),
            Err(Refusal::ColumnNotInOutput {
                column: "owner".to_string()
            })
        );
        let renamed = project(scan("docs", &["id", "owner"]), &[(col("owner"), "who")]);
        assert_eq!(
            prove(&renamed, "docs", &owner()),
            Err(Refusal::ColumnRenamed {
                column: "owner".to_string(),
                output: "who".to_string()
            })
        );
        let computed = project(
            scan("docs", &["id", "owner"]),
            &[(
                LogicalExpr::ScalarFunction {
                    fun: "upper".to_string(),
                    args: vec![col("owner")],
                },
                "owner",
            )],
        );
        assert_eq!(
            prove(&computed, "docs", &owner()),
            Err(Refusal::ColumnComputed {
                column: "owner".to_string()
            })
        );
    }

    #[test]
    fn a_limit_is_refused_and_a_table_not_read_is_no_concern() {
        let limited = LogicalPlan::Limit {
            input: Box::new(scan("docs", &["id", "owner"])),
            limit: 1,
            offset: 0,
            limit_param: None,
            offset_param: None,
        };
        assert_eq!(prove(&limited, "docs", &owner()), Err(Refusal::Limit));
        assert_eq!(prove(&limited, "other", &owner()), Ok(()));
    }

    fn aggregate(input: LogicalPlan, group_by: &[&str]) -> LogicalPlan {
        LogicalPlan::Aggregate {
            input: Box::new(input),
            group_by: group_by.iter().map(|c| col(c)).collect(),
            aggr_exprs: vec![LogicalExpr::AggregateFunction {
                fun: crate::sql::logical_plan::AggregateFunction::Count,
                args: vec![LogicalExpr::Wildcard],
                distinct: false,
            }],
            having: None,
        }
    }

    fn join(left: LogicalPlan, right: LogicalPlan, join_type: JoinType) -> LogicalPlan {
        LogicalPlan::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type,
            on: None,
            lateral: false,
        }
    }

    fn docs() -> LogicalPlan {
        scan("docs", &["id", "owner"])
    }

    #[test]
    fn an_aggregate_needs_the_column_as_a_grouping_key() {
        assert_eq!(prove(&aggregate(docs(), &["owner"]), "docs", &owner()), Ok(()));
        assert_eq!(
            prove(&aggregate(docs(), &["id"]), "docs", &owner()),
            Err(Refusal::NotGroupingKey {
                column: "owner".to_string()
            })
        );
        // One row for all rows: refused even for a policy that reads no column.
        assert_eq!(prove(&aggregate(docs(), &[]), "docs", &[]), Err(Refusal::UngroupedAggregate));
        // Grouped, and the policy reads no column: every group is all-or-nothing.
        assert_eq!(prove(&aggregate(docs(), &["id"]), "docs", &[]), Ok(()));
    }

    #[test]
    fn joins_are_accepted_unless_they_can_null_the_table_or_read_it_twice() {
        let other = || scan("tags", &["doc_id", "tag"]);
        assert_eq!(prove(&join(docs(), other(), JoinType::Inner), "docs", &owner()), Ok(()));
        assert_eq!(prove(&join(other(), docs(), JoinType::Cross), "docs", &owner()), Ok(()));
        // The table on the side an outer join keeps.
        assert_eq!(prove(&join(docs(), other(), JoinType::Left), "docs", &owner()), Ok(()));
        assert_eq!(prove(&join(other(), docs(), JoinType::Right), "docs", &owner()), Ok(()));
        // The table on the side an outer join can null.
        assert_eq!(
            prove(&join(other(), docs(), JoinType::Left), "docs", &owner()),
            Err(Refusal::OuterJoin)
        );
        assert_eq!(
            prove(&join(docs(), other(), JoinType::Right), "docs", &owner()),
            Err(Refusal::OuterJoin)
        );
        assert_eq!(
            prove(&join(docs(), other(), JoinType::Full), "docs", &owner()),
            Err(Refusal::OuterJoin)
        );
        assert_eq!(
            prove(&join(docs(), docs(), JoinType::Inner), "docs", &owner()),
            Err(Refusal::TableReadTwice)
        );
        // The other side has a column of the same name.
        assert_eq!(
            prove(
                &join(docs(), scan("people", &["owner"]), JoinType::Inner),
                "docs",
                &owner()
            ),
            Err(Refusal::AmbiguousColumn {
                column: "owner".to_string()
            })
        );
    }

    #[test]
    fn set_operations_other_than_union_all_of_like_branches_are_refused() {
        let union = |left: LogicalPlan, right: LogicalPlan, all: bool| LogicalPlan::Union {
            left: Box::new(left),
            right: Box::new(right),
            all,
        };
        assert_eq!(prove(&union(docs(), docs(), true), "docs", &owner()), Ok(()));
        assert_eq!(
            prove(&union(docs(), docs(), false), "docs", &owner()),
            Err(Refusal::SetOperation { operation: "UNION" })
        );
        assert_eq!(
            prove(&union(docs(), scan("archive", &["id", "owner"]), true), "docs", &owner()),
            Err(Refusal::UnionBranchWithoutTable)
        );
        let intersect = LogicalPlan::Intersect {
            left: Box::new(docs()),
            right: Box::new(scan("archive", &["id", "owner"])),
            all: false,
        };
        assert_eq!(
            prove(&intersect, "docs", &owner()),
            Err(Refusal::SetOperation { operation: "INTERSECT" })
        );
        let except = LogicalPlan::Except {
            left: Box::new(scan("archive", &["id", "owner"])),
            right: Box::new(docs()),
            all: false,
        };
        assert_eq!(
            prove(&except, "docs", &owner()),
            Err(Refusal::SetOperation { operation: "EXCEPT" })
        );
        assert_eq!(prove(&except, "unrelated", &owner()), Ok(()));
    }

    #[test]
    fn distinct_on_windows_subqueries_and_ctes_over_the_table_are_refused() {
        let mut distinct_on = project(docs(), &[(col("id"), "id"), (col("owner"), "owner")]);
        if let LogicalPlan::Project { distinct_on: d, .. } = &mut distinct_on {
            *d = Some(vec![col("owner")]);
        }
        assert_eq!(prove(&distinct_on, "docs", &owner()), Err(Refusal::DistinctOn));

        let window = |partition_by: Vec<LogicalExpr>| {
            project(
                docs(),
                &[
                    (col("owner"), "owner"),
                    (
                        LogicalExpr::WindowFunction {
                            fun: crate::sql::logical_plan::WindowFunctionType::RowNumber,
                            args: vec![],
                            partition_by,
                            order_by: vec![],
                            frame: None,
                        },
                        "n",
                    ),
                ],
            )
        };
        assert_eq!(prove(&window(vec![col("owner")]), "docs", &owner()), Ok(()));
        assert_eq!(
            prove(&window(vec![col("id")]), "docs", &owner()),
            Err(Refusal::WindowAcrossRows {
                column: Some("owner".to_string())
            })
        );

        let with_subquery = LogicalPlan::Filter {
            input: Box::new(scan("tags", &["doc_id"])),
            predicate: LogicalExpr::InSubquery {
                expr: Box::new(col("doc_id")),
                subquery: Box::new(project(docs(), &[(col("id"), "id")])),
                negated: false,
            },
        };
        assert_eq!(prove(&with_subquery, "docs", &owner()), Err(Refusal::SubqueryOverTable));
        assert_eq!(prove(&with_subquery, "tags", &[]), Ok(()));

        let cte = LogicalPlan::With {
            ctes: vec![("d".to_string(), Box::new(docs()), None)],
            query: Box::new(scan("d", &["id", "owner"])),
            recursive: false,
        };
        assert!(matches!(prove(&cte, "docs", &owner()), Err(Refusal::Unsupported { .. })));
    }

    #[test]
    fn every_refusal_says_why_and_names_its_column() {
        let refusal = Refusal::ColumnRenamed {
            column: "owner".to_string(),
            output: "who".to_string(),
        };
        assert_eq!(refusal.column(), Some("owner"));
        assert_eq!(
            refusal.to_string(),
            "column \"owner\" is not preserved: the view's output has it under the name \"who\""
        );
        assert_eq!(Refusal::Limit.column(), None);
        assert!(Refusal::OuterJoin.to_string().contains("outer join"));
    }

    #[test]
    fn the_tables_of_a_query_include_those_its_subqueries_read() {
        let plan = LogicalPlan::Filter {
            input: Box::new(join(docs(), scan("tags", &["doc_id"]), JoinType::Inner)),
            predicate: LogicalExpr::Exists {
                subquery: Box::new(scan("members", &["member"])),
                negated: false,
            },
        };
        assert_eq!(scanned_tables(&plan), ["docs", "tags", "members"]);
    }

    #[test]
    fn a_policy_reads_only_the_columns_outside_its_subqueries() {
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(col("owner")),
            op: crate::sql::BinaryOperator::And,
            right: Box::new(LogicalExpr::InSubquery {
                expr: Box::new(col("dept")),
                subquery: Box::new(project(
                    scan("m", &["dept", "member"]),
                    &[(col("dept"), "dept")],
                )),
                negated: false,
            }),
        };
        assert_eq!(policy_columns(&expr), ["owner", "dept"]);
        assert_eq!(
            policy_columns(&LogicalExpr::Literal(Value::Boolean(false))),
            Vec::<String>::new()
        );
    }
}
