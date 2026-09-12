//! GH#29 (candidate 3, M2+M3): the de-stamp pass a materialized view's plan
//! goes through before it is serialized.
//!
//! A derived table's or an expanded view's root projection carries
//! <code>[LogicalPlan::Project]::source_alias</code> — the alias its output is known
//! by at runtime — and every qualified reference to it (`s.id`) resolves
//! through that stamp. The stamp is NOT persisted (`#[serde(skip)]`: the
//! materialized-view plan is positional bincode and a new field would make
//! every plan written by an earlier release undecodable), so a plan that
//! still needs it would materialize once and fail at the first `REFRESH`
//! that re-executes the stored bytes.
//!
//! Candidate 2 refused every such plan up front (0A000) — including the
//! common, unshadowed `SELECT v.id FROM myview v` and `SELECT s.total FROM
//! (…) s` — and its guard walked expressions with `map_column_refs`, which
//! never descends into aggregate / window arguments, so `SELECT sum(s.x)
//! FROM (…) s` slipped through and broke at refresh. This pass replaces
//! both halves:
//!
//! * every `alias.col` that names a stamped entry is rewritten to the bare
//!   `col` when `col` is carried by EXACTLY ONE column of the referencing
//!   node's input (a join: both sides together), and every stamp is cleared,
//!   so the persisted plan resolves by name alone and round-trips through
//!   bincode unchanged;
//! * only a reference whose bare name is ALSO carried by another entry of
//!   the same join (the genuinely shadowed shape, `SELECT s.id FROM t JOIN
//!   (SELECT id FROM t) s …`) is refused, with the workaround named
//!   (`alias the column inside the sub-select`).
//!
//! The expression walk is an explicit, exhaustive match over
//! [`LogicalExpr`] — aggregate and window arguments, `PARTITION BY` /
//! `ORDER BY`, `CASE` arms, `IN` lists, array subscripts, tuples and the
//! sub-plans of scalar / `IN` / `EXISTS` subqueries are all visited — and it
//! is scoped: a node's expressions resolve against what that node's INPUT
//! makes visible (innermost first), so an inner base-table alias equal to a
//! derived alias (`(SELECT s.id AS x FROM t AS s) AS s`) is that inner base
//! table, not a false positive. A correlated reference from inside a
//! subquery to an outer stamped alias is rewritten only when the bare name
//! is unique in that outer input AND absent from every input between (the
//! executor binds a bare correlated name inner-first); otherwise refused.
//!
//! Executed on the plan that is then BOTH materialized and stored, so
//! `CREATE` and `REFRESH` run byte-identical plans: a shape this pass gets
//! wrong fails at `CREATE`, never at a later refresh.

use super::logical_plan::{LogicalExpr, LogicalPlan};
use super::scope;
use crate::{Result, Schema};
use std::sync::Arc;

/// Rewrite `plan` so it no longer depends on any `Project::source_alias`
/// stamp; see the module docs. Refuses (0A000, with the workaround named)
/// only a qualified reference to a stamped entry whose bare name another
/// entry of the same input also carries.
pub(crate) fn destamp_source_aliases(plan: LogicalPlan) -> Result<LogicalPlan> {
    destamp_plan(plan, &[])
}

/// What one node's expressions can see: the runtime qualifiers of its
/// input's entries, which of them are stamped, and the input's schema.
#[derive(Clone)]
struct Ctx {
    /// Every runtime qualifier an entry of this input answers to (a base
    /// table's alias and real name, a table function's alias or name, a
    /// stamped projection's alias).
    names: Vec<String>,
    /// The subset of `names` that are stamped derived-table / view aliases.
    stamps: Vec<String>,
    /// The input's output schema (a join: left ++ right), as the planner
    /// sees it — a stamped projection's columns carry `source_table =
    /// alias`, exactly like the executor's schema.
    schema: Arc<Schema>,
}

impl Ctx {
    fn for_input(input: &LogicalPlan) -> Self {
        let mut names = Vec::new();
        let mut stamps = Vec::new();
        collect_visible(input, &mut names, &mut stamps);
        Self {
            names,
            stamps,
            schema: input.schema(),
        }
    }

    fn for_join(left: &LogicalPlan, right: &LogicalPlan) -> Self {
        let mut names = Vec::new();
        let mut stamps = Vec::new();
        collect_visible(left, &mut names, &mut stamps);
        collect_visible(right, &mut names, &mut stamps);
        let mut columns = left.schema().columns.clone();
        columns.extend(right.schema().columns.iter().cloned());
        Self {
            names,
            stamps,
            schema: Arc::new(Schema { columns }),
        }
    }

    fn knows(&self, qualifier: &str) -> bool {
        self.names.iter().any(|n| n == qualifier)
    }

    fn is_stamp(&self, qualifier: &str) -> bool {
        self.stamps.iter().any(|s| s == qualifier)
    }

    fn carries(&self, column: &str) -> usize {
        self.schema.columns.iter().filter(|c| c.name == column).count()
    }
}

fn push_name(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|n| n == name) {
        names.push(name.to_string());
    }
}

/// The entries a node's input makes visible by qualifier, walking the
/// pass-through nodes (filters, sorts, limits, joins) but stopping at a
/// stamped projection — its inside is another query level — and at any
/// other projection or aggregate, whose output is its own.
fn collect_visible(plan: &LogicalPlan, names: &mut Vec<String>, stamps: &mut Vec<String>) {
    match plan {
        LogicalPlan::Scan { table_name, alias, .. } | LogicalPlan::FilteredScan { table_name, alias, .. } => {
            push_name(names, alias.as_deref().unwrap_or(table_name.as_str()));
            push_name(names, table_name);
        }
        LogicalPlan::TableFunction {
            function_name, alias, ..
        } => push_name(names, alias.as_deref().unwrap_or(function_name.as_str())),
        LogicalPlan::Project {
            source_alias: Some(alias),
            ..
        } => {
            push_name(names, alias);
            push_name(stamps, alias);
        }
        LogicalPlan::Filter { input, .. } | LogicalPlan::Sort { input, .. } | LogicalPlan::Limit { input, .. } => {
            collect_visible(input, names, stamps)
        }
        LogicalPlan::Join { left, right, .. } => {
            collect_visible(left, names, stamps);
            collect_visible(right, names, stamps);
        }
        _ => {}
    }
}

fn with_ctx(stack: &[Ctx], ctx: Ctx) -> Vec<Ctx> {
    let mut extended = Vec::with_capacity(stack.len() + 1);
    extended.extend(stack.iter().cloned());
    extended.push(ctx);
    extended
}

/// Resolve one qualified reference against the context stack, innermost
/// first. A qualifier that names a stamped entry is rewritten to the bare
/// column when that name is unique in that level's input and not carried by
/// any level between; any other qualifier (a base table, a table function,
/// something no level knows) is kept as written.
fn rewrite_ref(stack: &[Ctx], table: Option<String>, name: String) -> Result<LogicalExpr> {
    let Some(qualifier) = table else {
        return Ok(LogicalExpr::Column { table: None, name });
    };
    for (level, ctx) in stack.iter().enumerate().rev() {
        if !ctx.knows(&qualifier) {
            continue;
        }
        if !ctx.is_stamp(&qualifier) {
            return Ok(LogicalExpr::Column {
                table: Some(qualifier),
                name,
            });
        }
        let unique_here = ctx.carries(&name) == 1;
        let shadowed_inside = stack.iter().skip(level + 1).any(|inner| inner.carries(&name) > 0);
        if unique_here && !shadowed_inside {
            return Ok(LogicalExpr::Column { table: None, name });
        }
        return Err(scope::materialized_view_derived_alias_shadowed(&qualifier, &name));
    }
    Ok(LogicalExpr::Column {
        table: Some(qualifier),
        name,
    })
}

fn rewrite_box(expr: Box<LogicalExpr>, stack: &[Ctx]) -> Result<Box<LogicalExpr>> {
    Ok(Box::new(rewrite_expr(*expr, stack)?))
}

fn rewrite_opt_box(expr: Option<Box<LogicalExpr>>, stack: &[Ctx]) -> Result<Option<Box<LogicalExpr>>> {
    match expr {
        Some(expr) => Ok(Some(rewrite_box(expr, stack)?)),
        None => Ok(None),
    }
}

fn rewrite_vec(exprs: Vec<LogicalExpr>, stack: &[Ctx]) -> Result<Vec<LogicalExpr>> {
    exprs.into_iter().map(|e| rewrite_expr(e, stack)).collect()
}

fn rewrite_opt_vec(exprs: Option<Vec<LogicalExpr>>, stack: &[Ctx]) -> Result<Option<Vec<LogicalExpr>>> {
    match exprs {
        Some(exprs) => Ok(Some(rewrite_vec(exprs, stack)?)),
        None => Ok(None),
    }
}

fn rewrite_opt(expr: Option<LogicalExpr>, stack: &[Ctx]) -> Result<Option<LogicalExpr>> {
    match expr {
        Some(expr) => Ok(Some(rewrite_expr(expr, stack)?)),
        None => Ok(None),
    }
}

/// The exhaustive expression walk (no wildcard arm: a new `LogicalExpr`
/// variant must be classified here before this compiles).
fn rewrite_expr(expr: LogicalExpr, stack: &[Ctx]) -> Result<LogicalExpr> {
    Ok(match expr {
        LogicalExpr::Column { table, name } => rewrite_ref(stack, table, name)?,
        LogicalExpr::Literal(value) => LogicalExpr::Literal(value),
        LogicalExpr::BinaryExpr { left, op, right } => LogicalExpr::BinaryExpr {
            left: rewrite_box(left, stack)?,
            op,
            right: rewrite_box(right, stack)?,
        },
        LogicalExpr::UnaryExpr { op, expr } => LogicalExpr::UnaryExpr {
            op,
            expr: rewrite_box(expr, stack)?,
        },
        LogicalExpr::AggregateFunction { fun, args, distinct } => LogicalExpr::AggregateFunction {
            fun,
            args: rewrite_vec(args, stack)?,
            distinct,
        },
        LogicalExpr::ScalarFunction { fun, args } => LogicalExpr::ScalarFunction {
            fun,
            args: rewrite_vec(args, stack)?,
        },
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => LogicalExpr::Case {
            expr: rewrite_opt_box(expr, stack)?,
            when_then: when_then
                .into_iter()
                .map(|(when, then)| Ok((rewrite_expr(when, stack)?, rewrite_expr(then, stack)?)))
                .collect::<Result<Vec<_>>>()?,
            else_result: rewrite_opt_box(else_result, stack)?,
        },
        LogicalExpr::Cast { expr, data_type } => LogicalExpr::Cast {
            expr: rewrite_box(expr, stack)?,
            data_type,
        },
        LogicalExpr::IsNull { expr, is_null } => LogicalExpr::IsNull {
            expr: rewrite_box(expr, stack)?,
            is_null,
        },
        LogicalExpr::Between {
            expr,
            low,
            high,
            negated,
        } => LogicalExpr::Between {
            expr: rewrite_box(expr, stack)?,
            low: rewrite_box(low, stack)?,
            high: rewrite_box(high, stack)?,
            negated,
        },
        LogicalExpr::InList { expr, list, negated } => LogicalExpr::InList {
            expr: rewrite_box(expr, stack)?,
            list: rewrite_vec(list, stack)?,
            negated,
        },
        LogicalExpr::InSet { expr, values, negated } => LogicalExpr::InSet {
            expr: rewrite_box(expr, stack)?,
            values,
            negated,
        },
        LogicalExpr::ScalarSubquery { subquery } => LogicalExpr::ScalarSubquery {
            subquery: Box::new(destamp_plan(*subquery, stack)?),
        },
        LogicalExpr::InSubquery {
            expr,
            subquery,
            negated,
        } => LogicalExpr::InSubquery {
            expr: rewrite_box(expr, stack)?,
            subquery: Box::new(destamp_plan(*subquery, stack)?),
            negated,
        },
        LogicalExpr::Exists { subquery, negated } => LogicalExpr::Exists {
            subquery: Box::new(destamp_plan(*subquery, stack)?),
            negated,
        },
        LogicalExpr::DefaultValue => LogicalExpr::DefaultValue,
        LogicalExpr::Wildcard => LogicalExpr::Wildcard,
        LogicalExpr::Parameter { index } => LogicalExpr::Parameter { index },
        LogicalExpr::NewRow { column } => LogicalExpr::NewRow { column },
        LogicalExpr::OldRow { column } => LogicalExpr::OldRow { column },
        LogicalExpr::ArraySubscript { array, index } => LogicalExpr::ArraySubscript {
            array: rewrite_box(array, stack)?,
            index: rewrite_box(index, stack)?,
        },
        LogicalExpr::Tuple { items } => LogicalExpr::Tuple {
            items: rewrite_vec(items, stack)?,
        },
        LogicalExpr::WindowFunction {
            fun,
            args,
            partition_by,
            order_by,
            frame,
        } => LogicalExpr::WindowFunction {
            fun,
            args: rewrite_vec(args, stack)?,
            partition_by: rewrite_vec(partition_by, stack)?,
            order_by: order_by
                .into_iter()
                .map(|(expr, asc)| Ok((rewrite_expr(expr, stack)?, asc)))
                .collect::<Result<Vec<_>>>()?,
            frame,
        },
        LogicalExpr::BoundColumn { index, table, name } => LogicalExpr::BoundColumn { index, table, name },
    })
}

/// The plan walk over the read-only node set a view query is built from.
/// Each node's expressions are rewritten against the context its input
/// provides (computed BEFORE the input is rewritten, so the stamps are
/// still visible), then the input is rewritten with that context pushed —
/// a stamped projection is rebuilt without its stamp. Every other node
/// (DDL, DML, …) cannot appear in a view query and is returned unchanged.
fn destamp_plan(plan: LogicalPlan, stack: &[Ctx]) -> Result<LogicalPlan> {
    let leaf_ctx = matches!(plan, LogicalPlan::FilteredScan { .. }).then(|| Ctx::for_input(&plan));
    match plan {
        LogicalPlan::FilteredScan {
            table_name,
            alias,
            schema,
            projection,
            predicate,
            as_of,
        } => {
            let stack = with_ctx(
                stack,
                leaf_ctx.unwrap_or_else(|| Ctx {
                    names: Vec::new(),
                    stamps: Vec::new(),
                    schema: schema.clone(),
                }),
            );
            Ok(LogicalPlan::FilteredScan {
                table_name,
                alias,
                schema,
                projection,
                predicate: rewrite_opt(predicate, &stack)?,
                as_of,
            })
        }
        LogicalPlan::Filter { input, predicate } => {
            let stack = with_ctx(stack, Ctx::for_input(&input));
            Ok(LogicalPlan::Filter {
                predicate: rewrite_expr(predicate, &stack)?,
                input: Box::new(destamp_plan(*input, &stack)?),
            })
        }
        LogicalPlan::Project {
            input,
            exprs,
            aliases,
            distinct,
            distinct_on,
            source_alias: _,
        } => {
            let stack = with_ctx(stack, Ctx::for_input(&input));
            Ok(LogicalPlan::Project {
                exprs: rewrite_vec(exprs, &stack)?,
                aliases,
                distinct,
                distinct_on: rewrite_opt_vec(distinct_on, &stack)?,
                input: Box::new(destamp_plan(*input, &stack)?),
                source_alias: None,
            })
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggr_exprs,
            having,
        } => {
            let stack = with_ctx(stack, Ctx::for_input(&input));
            Ok(LogicalPlan::Aggregate {
                group_by: rewrite_vec(group_by, &stack)?,
                aggr_exprs: rewrite_vec(aggr_exprs, &stack)?,
                having: rewrite_opt(having, &stack)?,
                input: Box::new(destamp_plan(*input, &stack)?),
            })
        }
        LogicalPlan::Sort { input, exprs, asc } => {
            let stack = with_ctx(stack, Ctx::for_input(&input));
            Ok(LogicalPlan::Sort {
                exprs: rewrite_vec(exprs, &stack)?,
                asc,
                input: Box::new(destamp_plan(*input, &stack)?),
            })
        }
        LogicalPlan::Limit {
            input,
            limit,
            offset,
            limit_param,
            offset_param,
        } => Ok(LogicalPlan::Limit {
            input: Box::new(destamp_plan(*input, stack)?),
            limit,
            offset,
            limit_param,
            offset_param,
        }),
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            lateral,
        } => {
            let stack = with_ctx(stack, Ctx::for_join(&left, &right));
            Ok(LogicalPlan::Join {
                on: rewrite_opt(on, &stack)?,
                left: Box::new(destamp_plan(*left, &stack)?),
                right: Box::new(destamp_plan(*right, &stack)?),
                join_type,
                lateral,
            })
        }
        LogicalPlan::Union { left, right, all } => Ok(LogicalPlan::Union {
            left: Box::new(destamp_plan(*left, stack)?),
            right: Box::new(destamp_plan(*right, stack)?),
            all,
        }),
        LogicalPlan::Intersect { left, right, all } => Ok(LogicalPlan::Intersect {
            left: Box::new(destamp_plan(*left, stack)?),
            right: Box::new(destamp_plan(*right, stack)?),
            all,
        }),
        LogicalPlan::Except { left, right, all } => Ok(LogicalPlan::Except {
            left: Box::new(destamp_plan(*left, stack)?),
            right: Box::new(destamp_plan(*right, stack)?),
            all,
        }),
        LogicalPlan::TableFunction {
            function_name,
            args,
            alias,
            column_alias,
        } => Ok(LogicalPlan::TableFunction {
            function_name,
            args: rewrite_vec(args, stack)?,
            alias,
            column_alias,
        }),
        LogicalPlan::With { ctes, query, recursive } => Ok(LogicalPlan::With {
            ctes: ctes
                .into_iter()
                .map(|(name, cte_plan, columns)| Ok((name, Box::new(destamp_plan(*cte_plan, stack)?), columns)))
                .collect::<Result<Vec<_>>>()?,
            query: Box::new(destamp_plan(*query, stack)?),
            recursive,
        }),
        other => Ok(other),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::sql::logical_plan::{AggregateFunction, JoinType};
    use crate::{Column, DataType};

    fn column(name: &str) -> Column {
        Column {
            name: name.to_string(),
            data_type: DataType::Int4,
            nullable: true,
            primary_key: false,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
        }
    }

    fn scan(table: &str, alias: Option<&str>, columns: &[&str]) -> LogicalPlan {
        LogicalPlan::Scan {
            table_name: table.to_string(),
            alias: alias.map(str::to_string),
            schema: Arc::new(Schema {
                columns: columns.iter().map(|c| column(c)).collect(),
            }),
            projection: None,
            as_of: None,
        }
    }

    fn col(table: Option<&str>, name: &str) -> LogicalExpr {
        LogicalExpr::Column {
            table: table.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn stamped(input: LogicalPlan, names: &[&str], alias: &str) -> LogicalPlan {
        LogicalPlan::Project {
            input: Box::new(input),
            exprs: names.iter().map(|n| col(None, n)).collect(),
            aliases: names.iter().map(|n| n.to_string()).collect(),
            distinct: false,
            distinct_on: None,
            source_alias: Some(alias.to_string()),
        }
    }

    fn stamps_left(plan: &LogicalPlan) -> bool {
        match plan {
            LogicalPlan::Project {
                source_alias: Some(_), ..
            } => true,
            LogicalPlan::Project { input, .. }
            | LogicalPlan::Filter { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Aggregate { input, .. } => stamps_left(input),
            LogicalPlan::Join { left, right, .. } => stamps_left(left) || stamps_left(right),
            _ => false,
        }
    }

    #[test]
    fn unshadowed_alias_reference_becomes_bare_and_the_stamp_is_cleared() {
        // SELECT sum(s.x) FROM (SELECT x FROM t) s — the aggregate argument
        // the candidate-2 guard never saw.
        let plan = LogicalPlan::Project {
            input: Box::new(LogicalPlan::Aggregate {
                input: Box::new(stamped(scan("t", None, &["x"]), &["x"], "s")),
                group_by: vec![],
                aggr_exprs: vec![LogicalExpr::AggregateFunction {
                    fun: AggregateFunction::Sum,
                    args: vec![col(Some("s"), "x")],
                    distinct: false,
                }],
                having: None,
            }),
            exprs: vec![col(None, "agg_0")],
            aliases: vec!["sum".to_string()],
            distinct: false,
            distinct_on: None,
            source_alias: None,
        };
        let out = destamp_source_aliases(plan).expect("unshadowed: accepted");
        assert!(!stamps_left(&out));
        let LogicalPlan::Project { input, .. } = &out else {
            panic!("root");
        };
        let LogicalPlan::Aggregate { aggr_exprs, .. } = input.as_ref() else {
            panic!("aggregate");
        };
        assert_eq!(
            aggr_exprs[0],
            LogicalExpr::AggregateFunction {
                fun: AggregateFunction::Sum,
                args: vec![col(None, "x")],
                distinct: false,
            }
        );
    }

    #[test]
    fn shadowed_alias_reference_is_refused_with_the_workaround() {
        // SELECT s.id FROM t JOIN (SELECT id FROM t) s ON s.id = t.id
        let join = LogicalPlan::Join {
            left: Box::new(scan("t", None, &["id", "v"])),
            right: Box::new(stamped(scan("t", None, &["id", "v"]), &["id"], "s")),
            join_type: JoinType::Inner,
            on: Some(LogicalExpr::BinaryExpr {
                left: Box::new(col(Some("s"), "id")),
                op: crate::sql::logical_plan::BinaryOperator::Eq,
                right: Box::new(col(Some("t"), "id")),
            }),
            lateral: false,
        };
        let plan = LogicalPlan::Project {
            input: Box::new(join),
            exprs: vec![col(Some("s"), "id")],
            aliases: vec!["id".to_string()],
            distinct: false,
            distinct_on: None,
            source_alias: None,
        };
        let err = destamp_source_aliases(plan).expect_err("shadowed: refused").to_string();
        assert!(
            err.contains(scope::MATERIALIZED_VIEW_DERIVED_ALIAS_UNSUPPORTED),
            "{err}"
        );
        assert!(err.contains("alias the column inside the sub-select"), "{err}");
    }

    #[test]
    fn inner_base_alias_equal_to_the_derived_alias_is_not_a_false_positive() {
        // SELECT s.x FROM (SELECT s.id AS x FROM t AS s) AS s
        let inner = LogicalPlan::Project {
            input: Box::new(scan("t", Some("s"), &["id"])),
            exprs: vec![col(Some("s"), "id")],
            aliases: vec!["x".to_string()],
            distinct: false,
            distinct_on: None,
            source_alias: Some("s".to_string()),
        };
        let plan = LogicalPlan::Project {
            input: Box::new(inner),
            exprs: vec![col(Some("s"), "x")],
            aliases: vec!["x".to_string()],
            distinct: false,
            distinct_on: None,
            source_alias: None,
        };
        let out = destamp_source_aliases(plan).expect("inner alias is the base table");
        let LogicalPlan::Project { exprs, input, .. } = &out else {
            panic!("root");
        };
        assert_eq!(exprs[0], col(None, "x"));
        let LogicalPlan::Project {
            exprs: inner_exprs,
            source_alias,
            ..
        } = input.as_ref()
        else {
            panic!("inner");
        };
        assert_eq!(
            inner_exprs[0],
            col(Some("s"), "id"),
            "the inner `s.id` names the base table"
        );
        assert!(source_alias.is_none());
    }

    #[test]
    fn plan_without_stamps_is_returned_unchanged() {
        let plan = LogicalPlan::Project {
            input: Box::new(scan("t", None, &["id"])),
            exprs: vec![col(Some("t"), "id")],
            aliases: vec!["id".to_string()],
            distinct: false,
            distinct_on: None,
            source_alias: None,
        };
        let out = destamp_source_aliases(plan.clone()).expect("no stamps");
        assert_eq!(out, plan);
    }
}
