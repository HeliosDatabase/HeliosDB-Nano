//! `RETURNING` projection — ONE binding of the item list that feeds BOTH the
//! wire `RowDescription` (via [`ReturningProjection::schema`]) and every
//! `DataRow` (via [`ReturningProjection::project`]).
//!
//! # Why one binding
//!
//! Through v4.31.1 the two halves were computed by two different functions
//! from two different premises: `EmbeddedDatabase::returning_schema` typed
//! every non-bare item as `DataType::Text` (GH#23 — Prisma's P2023 on a
//! boolean that arrived as `t` under OID 25), while
//! `project_returning_columns` evaluated the same item through the row
//! evaluator and mapped ANY failure to `Value::Null` — so a qualifier that
//! missed the catalog's stamped `source_table_name` (`Typed."n" AS "cnt"`,
//! `x."n" AS "c"`) arrived under exactly the name the client bound, carrying
//! NULL for a `NOT NULL` column, with no error anywhere.
//!
//! Here every item is resolved ONCE, before any row is written: column
//! references (bare, aliased or qualified) bind to a catalog column by index —
//! the catalog column's own type, nullability and `primary_key` — and genuine
//! expressions are typed by the SELECT list's own typer
//! (`TypeInference::to_column`). `schema()` column *i* and `project()` value
//! *i* are the SAME `ReturningBinding`, so the advertised type and the sent
//! value cannot come from different premises. A name that resolves to nothing
//! is REFUSED (SQLSTATE 42703 on the wire) — never substituted with NULL.
//!
//! The planner's `ReturningItem::Column` vs `ReturningItem::Expression`
//! choice is a lowering convenience and is not load-bearing here: `bind`
//! reads the SHAPE of the item, so `"n"`, `"n" AS "cnt"`, `t."n"` and
//! `t."n" AS "cnt"` all become the same `CatalogColumn` binding.

use super::evaluator::{map_column_refs, Evaluator};
use super::logical_plan::{LogicalExpr, ReturningItem};
use super::type_inference::TypeInference;
use crate::{Column, Error, Result, Schema, Tuple, Value};
use std::sync::Arc;

/// PostgreSQL's refusal of an aggregate anywhere in a RETURNING list
/// (parser/parse_agg.c, `EXPR_KIND_RETURNING`). Raised by
/// [`ReturningProjection::bind`] BEFORE any row is written; the wire
/// classifier anchors on this exact text → SQLSTATE 42803 grouping_error.
///
/// Through GH#23 candidate 2 `RETURNING count(*)` was lowered to an
/// `Expression` item, survived bind, and then failed the whole statement at
/// project time with the evaluator's `Expression not yet implemented:
/// AggregateFunction` (XX000); on v4.31.1 it silently returned NULL under the
/// name `count`. Neither is PostgreSQL's answer.
pub(crate) const AGGREGATE_IN_RETURNING: &str = "aggregate functions are not allowed in RETURNING";

/// The window-function twin of [`AGGREGATE_IN_RETURNING`]
/// (parser/parse_func.c `transformWindowFuncCall`) → SQLSTATE 42P20
/// windowing_error, PostgreSQL's class for a misplaced window call.
pub(crate) const WINDOW_IN_RETURNING: &str = "window functions are not allowed in RETURNING";

/// One output column of a `RETURNING` list, bound against the target table.
#[derive(Debug, Clone)]
pub(crate) enum ReturningBinding {
    /// The item IS target-table column `index`; `column` is that catalog
    /// column renamed to the output name (the item alias, or the bare name).
    CatalogColumn {
        /// Position of the column in the target table's schema / row tuple.
        index: usize,
        /// The catalog column, renamed to the output name.
        column: Column,
    },
    /// A genuine expression over the target table, every column reference
    /// already resolved (qualifier dropped); `column` is
    /// `expr.to_column(alias, table_schema)` — exactly how the SELECT list
    /// types the same expression.
    Expression {
        /// The resolved expression, evaluated per row.
        expr: LogicalExpr,
        /// The output column: alias + inferred type.
        column: Column,
    },
}

impl ReturningBinding {
    fn column(&self) -> &Column {
        match self {
            ReturningBinding::CatalogColumn { column, .. } | ReturningBinding::Expression { column, .. } => column,
        }
    }
}

/// A bound `RETURNING` list: the single source of truth for the output
/// schema AND the projected values of one DML statement.
pub(crate) struct ReturningProjection {
    bindings: Vec<ReturningBinding>,
    /// Built ONCE per statement, and only when some binding is an
    /// [`ReturningBinding::Expression`]; `RETURNING *` / `RETURNING id` never
    /// construct an evaluator at all.
    evaluator: Option<Evaluator>,
}

/// `Evaluator` is not `Debug`; render the bindings and whether an evaluator was
/// built, which is what a failing test needs to see (`unwrap_err` on a
/// `Result<ReturningProjection>` requires `Debug` on the Ok type).
impl std::fmt::Debug for ReturningProjection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReturningProjection")
            .field("bindings", &self.bindings)
            .field("has_evaluator", &self.evaluator.is_some())
            .finish_non_exhaustive()
    }
}

impl ReturningProjection {
    /// Bind `items` against `table_schema` (the target table's catalog
    /// schema). Refuses — never invents — anything it cannot resolve:
    ///
    /// * a column reference naming no column of the target table →
    ///   `Column '…' does not exist in RETURNING` (42703 on the wire);
    /// * an `EXCLUDED.col` reference → `Table 'excluded' not found …`
    ///   (42P01): PostgreSQL forbids `EXCLUDED` in RETURNING, and answering
    ///   with the stored row's column would be silently wrong.
    ///
    /// * an aggregate (`count(*)`, `coalesce(max(n), 0)`) or a window call
    ///   (`row_number() OVER ()`) anywhere in an item →
    ///   [`AGGREGATE_IN_RETURNING`] (42803) / [`WINDOW_IN_RETURNING`] (42P20),
    ///   PostgreSQL's own refusals. A RETURNING list is a per-row projection
    ///   of the written rows; it has no group to aggregate over and no window
    ///   to rank in. Refused here, before the first write, on both executor
    ///   families — never evaluated to NULL, never failed after the write.
    ///
    /// Any other qualifier (`"public"."T"."c"`, a case-folded `T.c`, a FROM
    /// alias `x.c`) resolves by the bare column name — the rule
    /// `Planner::convert_returning` already applies to the unaliased spelling:
    /// in a single-table DML RETURNING list the qualifier cannot select a
    /// different column. Tightening that (refusing an unknown qualifier) is
    /// GH#29's family and belongs in this function when it lands.
    pub(crate) fn bind(table_schema: &Schema, items: &[ReturningItem]) -> Result<Self> {
        Self::bind_with_parameters(table_schema, items, &[])
    }

    /// [`bind`](Self::bind) for the params executor family: `$n` inside a
    /// RETURNING expression evaluates against `params`.
    pub(crate) fn bind_with_parameters(
        table_schema: &Schema,
        items: &[ReturningItem],
        params: &[Value],
    ) -> Result<Self> {
        let mut bindings: Vec<ReturningBinding> = Vec::with_capacity(items.len());
        for item in items {
            match item {
                // Expanded IN SEQUENCE with sibling items, so `RETURNING *, "n"`
                // describes N+1 fields and sends N+1 values.
                ReturningItem::Wildcard => {
                    for (index, column) in table_schema.columns.iter().enumerate() {
                        bindings.push(ReturningBinding::CatalogColumn {
                            index,
                            column: column.clone(),
                        });
                    }
                }
                ReturningItem::Column(name) => {
                    let (index, column) = catalog_column(table_schema, None, name)?;
                    bindings.push(ReturningBinding::CatalogColumn { index, column });
                }
                ReturningItem::Expression { expr, alias } => {
                    refuse_aggregates_and_windows(expr)?;
                    match resolve_returning_refs(expr.clone(), table_schema)? {
                        // `col AS alias` (any qualification): the catalog column,
                        // renamed — bit-identical type/nullability/primary_key to
                        // the unaliased spelling.
                        LogicalExpr::Column { table: None, name } => {
                            let (index, mut column) = catalog_column(table_schema, None, &name)?;
                            column.name = alias.clone();
                            bindings.push(ReturningBinding::CatalogColumn { index, column });
                        }
                        // A real expression: typed exactly as the SELECT list
                        // types it. `to_column` degrades an un-inferable
                        // expression to `Text` — the one OID a client cannot
                        // mis-decode into a wrong value.
                        expr => {
                            let column = expr.to_column(alias.clone(), table_schema);
                            bindings.push(ReturningBinding::Expression { expr, column });
                        }
                    }
                }
            }
        }

        let evaluator = if bindings
            .iter()
            .any(|b| matches!(b, ReturningBinding::Expression { .. }))
        {
            Some(Evaluator::with_parameters(
                Arc::new(table_schema.clone()),
                params.to_vec(),
            ))
        } else {
            None
        };

        Ok(Self { bindings, evaluator })
    }

    /// The output schema — what the wire advertises in `RowDescription`.
    /// Column *i* is `bindings[i]`.
    pub(crate) fn schema(&self) -> Schema {
        Schema::new(self.bindings.iter().map(|b| b.column().clone()).collect())
    }

    /// Project one written/deleted row. Value *i* is `bindings[i]` — the same
    /// binding [`schema`](Self::schema) described. Never substitutes NULL: a
    /// tuple shorter than a bound index is an engine invariant break (an
    /// error, XX000 on the wire), and an expression's runtime error
    /// (`RETURNING "n"/0`) propagates as PostgreSQL's does.
    pub(crate) fn project(&self, tuple: &Tuple) -> Result<Tuple> {
        let mut values = Vec::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            match binding {
                ReturningBinding::CatalogColumn { index, column } => {
                    let value = tuple.values.get(*index).cloned().ok_or_else(|| {
                        Error::query_execution(format!(
                            "RETURNING: row has {} values but column {} ('{}') was expected",
                            tuple.values.len(),
                            index,
                            column.name
                        ))
                    })?;
                    values.push(value);
                }
                ReturningBinding::Expression { expr, .. } => {
                    let evaluator = self
                        .evaluator
                        .as_ref()
                        .ok_or_else(|| Error::internal("RETURNING: expression binding without an evaluator"))?;
                    values.push(evaluator.evaluate(expr, tuple)?);
                }
            }
        }
        Ok(Tuple {
            values,
            row_id: tuple.row_id,
            branch_id: tuple.branch_id,
        })
    }
}

/// Resolve a bare column name against the target table, returning its index
/// and a clone of the catalog column. `qualifier` is only used to spell the
/// refusal the way the user wrote the reference.
fn catalog_column(table_schema: &Schema, qualifier: Option<&str>, name: &str) -> Result<(usize, Column)> {
    let index = table_schema
        .get_column_index(name)
        .ok_or_else(|| undefined_column(qualifier, name))?;
    let column = table_schema
        .columns
        .get(index)
        .cloned()
        .ok_or_else(|| undefined_column(qualifier, name))?;
    Ok((index, column))
}

/// `Column '<q.name>' does not exist in RETURNING` — classified as 42703
/// undefined_column by `sqlstate_for_query_execution_message`.
fn undefined_column(qualifier: Option<&str>, name: &str) -> Error {
    let spelled = match qualifier {
        Some(q) => format!("{q}.{name}"),
        None => name.to_string(),
    };
    Error::query_execution(format!("Column '{spelled}' does not exist in RETURNING"))
}

/// `EXCLUDED.<col>` in a RETURNING list — classified as 42P01 undefined_table
/// (PostgreSQL: `missing FROM-clause entry for table "excluded"`, also 42P01).
fn excluded_in_returning(name: &str) -> Error {
    Error::query_execution(format!(
        "Table 'excluded' not found in RETURNING: EXCLUDED.{name} is only valid in ON CONFLICT DO UPDATE SET"
    ))
}

/// Refuse an aggregate or window function node ANYWHERE in a RETURNING
/// expression — top level, inside a scalar function's arguments, a CASE
/// branch, a cast, an operator — with PostgreSQL's wording
/// ([`AGGREGATE_IN_RETURNING`] / [`WINDOW_IN_RETURNING`]).
///
/// Descends every child expression except sub-plans: an aggregate INSIDE a
/// scalar / IN / EXISTS subquery (`RETURNING (SELECT count(*) FROM t)`)
/// belongs to that subquery's own scope and PostgreSQL allows it. The
/// evaluator has no row-by-row arm for either node (`Evaluator::evaluate`
/// returns `Expression not yet implemented: AggregateFunction` / `Window
/// functions must be evaluated by WindowOperator`), so without this check
/// the statement failed AFTER its rows were written, or — through v4.31.1 —
/// silently projected NULL.
fn refuse_aggregates_and_windows(expr: &LogicalExpr) -> Result<()> {
    let check = refuse_aggregates_and_windows;
    match expr {
        LogicalExpr::AggregateFunction { .. } => Err(Error::query_execution(AGGREGATE_IN_RETURNING)),
        LogicalExpr::WindowFunction { .. } => Err(Error::query_execution(WINDOW_IN_RETURNING)),
        LogicalExpr::BinaryExpr { left, right, .. } => {
            check(left)?;
            check(right)
        }
        LogicalExpr::UnaryExpr { expr, .. }
        | LogicalExpr::Cast { expr, .. }
        | LogicalExpr::IsNull { expr, .. }
        | LogicalExpr::InSet { expr, .. }
        // The sub-plan is its own aggregation scope; only the probe expression
        // belongs to the RETURNING list.
        | LogicalExpr::InSubquery { expr, .. } => check(expr),
        LogicalExpr::Between { expr, low, high, .. } => {
            check(expr)?;
            check(low)?;
            check(high)
        }
        LogicalExpr::InList { expr, list, .. } => {
            check(expr)?;
            list.iter().try_for_each(check)
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            if let Some(operand) = expr {
                check(operand)?;
            }
            for (when, then) in when_then {
                check(when)?;
                check(then)?;
            }
            match else_result {
                Some(otherwise) => check(otherwise),
                None => Ok(()),
            }
        }
        LogicalExpr::ScalarFunction { args, .. } | LogicalExpr::Tuple { items: args } => args.iter().try_for_each(check),
        LogicalExpr::ArraySubscript { array, index } => {
            check(array)?;
            check(index)
        }
        // Leaves, and sub-plans that own their aggregates.
        LogicalExpr::Column { .. }
        | LogicalExpr::BoundColumn { .. }
        | LogicalExpr::Literal(_)
        | LogicalExpr::ScalarSubquery { .. }
        | LogicalExpr::Exists { .. }
        | LogicalExpr::DefaultValue
        | LogicalExpr::Wildcard
        | LogicalExpr::Parameter { .. }
        | LogicalExpr::NewRow { .. }
        | LogicalExpr::OldRow { .. } => Ok(()),
    }
}

/// Walk every `LogicalExpr::Column` of a RETURNING expression and resolve it
/// against the target table: a hit is rewritten to `Column { table: None }`
/// (the qualifier is dropped — see [`ReturningProjection::bind`]), a miss or
/// an `EXCLUDED` qualifier is recorded and refused after the walk. Sub-plans,
/// aggregates and window functions are not descended into (same non-descent
/// set as the evaluator's own binder, `bind_expr_columns`).
fn resolve_returning_refs(expr: LogicalExpr, table_schema: &Schema) -> Result<LogicalExpr> {
    let mut failure: Option<Error> = None;
    let resolved = map_column_refs(expr, &mut |table, name| {
        if failure.is_none() {
            if table.as_deref().is_some_and(|q| q.eq_ignore_ascii_case("excluded")) {
                failure = Some(excluded_in_returning(&name));
            } else if table_schema.get_column_index(&name).is_some() {
                return LogicalExpr::Column { table: None, name };
            } else {
                failure = Some(undefined_column(table.as_deref(), &name));
            }
        }
        LogicalExpr::Column { table, name }
    });
    match failure {
        Some(e) => Err(e),
        None => Ok(resolved),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::sql::logical_plan::BinaryOperator;
    use crate::DataType;

    fn typed_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::Uuid).with_source_table("Typed"),
            Column::new("isStaff", DataType::Boolean).with_source_table("Typed"),
            Column::new("n", DataType::Int4).with_source_table("Typed"),
            Column::new("note", DataType::Text).with_source_table("Typed"),
        ])
    }

    fn col(table: Option<&str>, name: &str) -> LogicalExpr {
        LogicalExpr::Column {
            table: table.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn row() -> Tuple {
        Tuple::new(vec![
            Value::Uuid(uuid::Uuid::nil()),
            Value::Boolean(true),
            Value::Int4(7),
            Value::String("hello".into()),
        ])
    }

    #[test]
    fn aliased_column_keeps_catalog_type_and_value_under_any_qualifier() {
        let schema = typed_schema();
        for qualifier in [None, Some("Typed"), Some("typed"), Some("x"), Some("public.Typed")] {
            let items = vec![
                ReturningItem::Expression {
                    expr: col(qualifier, "n"),
                    alias: "cnt".into(),
                },
                ReturningItem::Expression {
                    expr: col(qualifier, "isStaff"),
                    alias: "staff".into(),
                },
            ];
            let p = ReturningProjection::bind(&schema, &items).unwrap_or_else(|e| panic!("{qualifier:?}: {e}"));
            let s = p.schema();
            assert_eq!(s.columns[0].name, "cnt");
            assert_eq!(s.columns[0].data_type, DataType::Int4, "{qualifier:?}");
            assert_eq!(s.columns[1].name, "staff");
            assert_eq!(s.columns[1].data_type, DataType::Boolean, "{qualifier:?}");
            let out = p.project(&row()).unwrap();
            assert_eq!(out.values, vec![Value::Int4(7), Value::Boolean(true)], "{qualifier:?}");
            assert!(p.evaluator.is_none(), "a renamed catalog column needs no evaluator");
        }
    }

    #[test]
    fn expression_is_typed_like_the_select_list() {
        let schema = typed_schema();
        let expr = LogicalExpr::BinaryExpr {
            left: Box::new(col(Some("typed"), "n")),
            op: BinaryOperator::Plus,
            right: Box::new(LogicalExpr::Literal(Value::Int4(1))),
        };
        let items = vec![ReturningItem::Expression {
            expr: expr.clone(),
            alias: "n1".into(),
        }];
        let p = ReturningProjection::bind(&schema, &items).unwrap();
        let expected = expr.to_column("n1".into(), &schema);
        assert_eq!(p.schema().columns[0].data_type, expected.data_type);
        assert_eq!(p.project(&row()).unwrap().values, vec![Value::Int4(8)]);
    }

    #[test]
    fn wildcard_expands_in_sequence_with_siblings() {
        let schema = typed_schema();
        let items = vec![ReturningItem::Wildcard, ReturningItem::Column("n".into())];
        let p = ReturningProjection::bind(&schema, &items).unwrap();
        assert_eq!(p.schema().columns.len(), 5);
        let out = p.project(&row()).unwrap();
        assert_eq!(out.values.len(), 5);
        assert_eq!(out.values[4], Value::Int4(7));
    }

    #[test]
    fn unknown_column_is_refused_at_bind_never_null() {
        let schema = typed_schema();
        for items in [
            vec![ReturningItem::Column("nosuch".into())],
            vec![ReturningItem::Expression {
                expr: col(Some("x"), "nosuch"),
                alias: "c".into(),
            }],
            vec![ReturningItem::Expression {
                expr: LogicalExpr::BinaryExpr {
                    left: Box::new(col(None, "n")),
                    op: BinaryOperator::Plus,
                    right: Box::new(col(None, "nosuch")),
                },
                alias: "c".into(),
            }],
        ] {
            let err = ReturningProjection::bind(&schema, &items).expect_err("must refuse");
            let msg = err.to_string();
            assert!(msg.contains("Column '") && msg.contains("does not exist"), "{msg}");
        }
    }

    #[test]
    fn excluded_qualifier_is_refused() {
        let schema = typed_schema();
        let items = vec![ReturningItem::Expression {
            expr: col(Some("EXCLUDED"), "n"),
            alias: "c".into(),
        }];
        let err = ReturningProjection::bind(&schema, &items).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("Table 'excluded' not found"), "{msg}");
        assert!(!msg.to_ascii_lowercase().contains("column '"), "{msg}");
    }

    #[test]
    fn short_tuple_is_an_error_not_a_null() {
        let schema = typed_schema();
        let items = vec![ReturningItem::Column("note".into())];
        let p = ReturningProjection::bind(&schema, &items).unwrap();
        let short = Tuple::new(vec![Value::Uuid(uuid::Uuid::nil())]);
        assert!(p.project(&short).is_err());
    }

    #[test]
    fn runtime_expression_error_propagates() {
        let schema = typed_schema();
        let items = vec![ReturningItem::Expression {
            expr: LogicalExpr::BinaryExpr {
                left: Box::new(col(None, "n")),
                op: BinaryOperator::Divide,
                right: Box::new(LogicalExpr::Literal(Value::Int4(0))),
            },
            alias: "c".into(),
        }];
        let p = ReturningProjection::bind(&schema, &items).unwrap();
        assert!(
            p.project(&row()).is_err(),
            "division by zero must surface, not become NULL"
        );
    }

    /// GH#23 candidate 3: an aggregate or window node anywhere in an item is
    /// refused AT BIND — PostgreSQL's wording — so no executor family can
    /// write a row first and fail (or NULL) afterwards.
    #[test]
    fn aggregates_and_windows_are_refused_at_bind_with_postgres_wording() {
        use super::super::logical_plan::{AggregateFunction, WindowFunctionType};
        let schema = typed_schema();
        let count_star = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Count,
            args: vec![LogicalExpr::Wildcard],
            distinct: false,
        };
        let max_n = LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Max,
            args: vec![col(None, "n")],
            distinct: false,
        };
        let row_number = LogicalExpr::WindowFunction {
            fun: WindowFunctionType::RowNumber,
            args: vec![],
            partition_by: vec![],
            order_by: vec![],
            frame: None,
        };
        let bind_one = |expr: LogicalExpr| {
            let items = vec![ReturningItem::Expression {
                expr,
                alias: "c".into(),
            }];
            match ReturningProjection::bind(&schema, &items).expect_err("must refuse at bind") {
                Error::QueryExecution(message) => message,
                other => panic!("must be a QueryExecution error (the wire classifier's input), got {other:?}"),
            }
        };

        // Top level, nested in a scalar function, nested in an operator, in a
        // CASE branch, under a cast: every shape is the same refusal.
        assert_eq!(bind_one(count_star.clone()), AGGREGATE_IN_RETURNING);
        assert_eq!(
            bind_one(LogicalExpr::ScalarFunction {
                fun: "coalesce".into(),
                args: vec![max_n.clone(), LogicalExpr::Literal(Value::Int4(0))],
            }),
            AGGREGATE_IN_RETURNING
        );
        assert_eq!(
            bind_one(LogicalExpr::BinaryExpr {
                left: Box::new(col(None, "n")),
                op: BinaryOperator::Plus,
                right: Box::new(count_star.clone()),
            }),
            AGGREGATE_IN_RETURNING
        );
        assert_eq!(
            bind_one(LogicalExpr::Case {
                expr: None,
                when_then: vec![(LogicalExpr::Literal(Value::Boolean(true)), max_n.clone())],
                else_result: None,
            }),
            AGGREGATE_IN_RETURNING
        );
        assert_eq!(
            bind_one(LogicalExpr::Cast {
                expr: Box::new(row_number.clone()),
                data_type: crate::DataType::Int8,
            }),
            WINDOW_IN_RETURNING
        );
        assert_eq!(bind_one(row_number), WINDOW_IN_RETURNING);

        // A sibling item does not rescue the list: the whole bind fails, so
        // there is no projection to write rows through.
        let items = vec![
            ReturningItem::Wildcard,
            ReturningItem::Expression {
                expr: count_star,
                alias: "c".into(),
            },
        ];
        assert!(ReturningProjection::bind(&schema, &items).is_err());

        // GUARD: an ordinary expression over the row still binds.
        let items = vec![ReturningItem::Expression {
            expr: LogicalExpr::ScalarFunction {
                fun: "coalesce".into(),
                args: vec![col(None, "n"), LogicalExpr::Literal(Value::Int4(0))],
            },
            alias: "c".into(),
        }];
        assert!(ReturningProjection::bind(&schema, &items).is_ok());
    }
}
