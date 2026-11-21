//! Stored generated columns: `GENERATED ALWAYS AS (expr) STORED`.
//!
//! # How a generated column is recorded
//!
//! * The column's stored default is the generation expression inside a marker
//!   call ([`GENERATED_STORED_FN`]). The schema is what every reader, a standby,
//!   a branch and a dump already carry, so the expression travels with it and no
//!   record of its own is needed. The marker is not a function: a write that
//!   reaches the plain default fill with the column omitted fails on it.
//! * The table holds one CHECK constraint per generated column,
//!   `column IS NOT DISTINCT FROM CAST(expr AS type)` ([`implicit_check`]). Every
//!   write path that validates CHECK constraints therefore verifies the stored
//!   value, whatever computed it: a path that does not compute the column refuses
//!   the row instead of storing a stale value. Catalog views and dumps leave the
//!   constraint out ([`is_implicit_check`]).
//!
//! # How the value is computed
//!
//! At plan time. `INSERT … VALUES` gets the column appended to its column list,
//! with the generation expression over the row's own value expressions
//! ([`rewrite_insert`]); `UPDATE` and `ON CONFLICT DO UPDATE` get an assignment of
//! the column whenever they assign a column it reads ([`rewrite_assignments`]).
//! A direct write of the column, other than `DEFAULT`, is refused (428C9).
//! `ALTER TABLE … ADD COLUMN … GENERATED` computes the value of every existing
//! row in the statement's write group (`alter_table`).
//!
//! # What is refused
//!
//! `VIRTUAL` (0A000); a generation expression with a subquery, an aggregate, a
//! window function, a parameter, a function that is not immutable, a reference to
//! another generated column or to a column the table does not have (42P17).

use crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER;
use crate::sql::logical_plan::{BinaryOperator, ColumnDef, LogicalExpr, LogicalPlan, TableConstraint};
use crate::{Column, DataType, Error, Result, Schema, Value};

/// The marker call a generated column's stored default is wrapped in.
pub(crate) const GENERATED_STORED_FN: &str = "helios_generated_stored";

/// In the message of a refused direct write of a generated column (SQLSTATE 428C9).
pub const GENERATED_COLUMN_WRITE_MARKER: &str = "is a generated column and can only be given DEFAULT";

/// In the message of a refused generation expression (SQLSTATE 42P17).
pub const GENERATION_EXPRESSION_INVALID_MARKER: &str = "is not allowed in a generation expression";

fn invalid(table: &str, column: &str, what: impl std::fmt::Display) -> Error {
    Error::query_execution(format!(
        "{what} {GENERATION_EXPRESSION_INVALID_MARKER} (generated column \"{column}\" of relation \"{table}\")"
    ))
}

fn direct_write(table: &str, column: &str) -> Error {
    Error::query_execution(format!(
        "column \"{column}\" of relation \"{table}\" {GENERATED_COLUMN_WRITE_MARKER}"
    ))
}

/// `expr` as a generated column's stored default.
pub(crate) fn wrap(expr: LogicalExpr) -> LogicalExpr {
    LogicalExpr::ScalarFunction {
        fun: GENERATED_STORED_FN.to_string(),
        args: vec![expr],
    }
}

/// The generation expression inside a stored default, when it is one.
pub(crate) fn unwrap(default: &LogicalExpr) -> Option<&LogicalExpr> {
    match default {
        LogicalExpr::ScalarFunction { fun, args } if fun == GENERATED_STORED_FN => args.first(),
        _ => None,
    }
}

/// Is this stored default (the catalog's JSON) a generation expression?
pub(crate) fn is_generated_default(default_json: Option<&str>) -> bool {
    generation_expr(default_json).is_some()
}

/// The generation expression of a stored default (the catalog's JSON).
pub(crate) fn generation_expr(default_json: Option<&str>) -> Option<LogicalExpr> {
    let json = default_json?;
    if !json.contains(GENERATED_STORED_FN) {
        return None;
    }
    let default: LogicalExpr = serde_json::from_str(json).ok()?;
    unwrap(&default).cloned()
}

/// Does the generated column with this stored default read `column`?
pub(crate) fn generated_reads(default_json: Option<&str>, column: &str) -> bool {
    generation_expr(default_json).is_some_and(|expr| {
        let mut read = Vec::new();
        columns_read(&expr, &mut read);
        read.iter().any(|name| name.eq_ignore_ascii_case(column))
    })
}

/// The generated columns of `schema`: position, column, generation expression.
fn generated_columns(schema: &Schema) -> Vec<(usize, &Column, LogicalExpr)> {
    schema
        .columns
        .iter()
        .enumerate()
        .filter_map(|(position, column)| {
            generation_expr(column.default_expr.as_deref()).map(|expr| (position, column, expr))
        })
        .collect()
}

/// The generation expression as SQL text, for catalog views.
pub(crate) fn generation_sql(default_json: Option<&str>) -> Option<String> {
    generation_expr(default_json).map(|expr| expr.to_default_sql())
}

/// The names of the columns `expr` reads.
pub(crate) fn columns_read(expr: &LogicalExpr, out: &mut Vec<String>) {
    match expr {
        LogicalExpr::Column { name, .. } | LogicalExpr::BoundColumn { name, .. } => out.push(name.clone()),
        LogicalExpr::BinaryExpr { left, right, .. } => {
            columns_read(left, out);
            columns_read(right, out);
        }
        LogicalExpr::UnaryExpr { expr, .. }
        | LogicalExpr::Cast { expr, .. }
        | LogicalExpr::IsNull { expr, .. }
        | LogicalExpr::InSet { expr, .. } => columns_read(expr, out),
        LogicalExpr::ScalarFunction { args, .. } => args.iter().for_each(|arg| columns_read(arg, out)),
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            if let Some(expr) = expr {
                columns_read(expr, out);
            }
            for (when, then) in when_then {
                columns_read(when, out);
                columns_read(then, out);
            }
            if let Some(else_result) = else_result {
                columns_read(else_result, out);
            }
        }
        LogicalExpr::Between { expr, low, high, .. } => {
            columns_read(expr, out);
            columns_read(low, out);
            columns_read(high, out);
        }
        LogicalExpr::InList { expr, list, .. } => {
            columns_read(expr, out);
            list.iter().for_each(|item| columns_read(item, out));
        }
        LogicalExpr::ArraySubscript { array, index } => {
            columns_read(array, out);
            columns_read(index, out);
        }
        LogicalExpr::Tuple { items } => items.iter().for_each(|item| columns_read(item, out)),
        _ => {}
    }
}

/// `expr` with each column reference `with` answers for replaced by its answer.
fn substitute(expr: &LogicalExpr, with: &dyn Fn(&str) -> Option<LogicalExpr>) -> LogicalExpr {
    let sub = |inner: &LogicalExpr| substitute(inner, with);
    let boxed = |inner: &LogicalExpr| Box::new(substitute(inner, with));
    match expr {
        LogicalExpr::Column { name, .. } | LogicalExpr::BoundColumn { name, .. } => {
            with(name).unwrap_or_else(|| expr.clone())
        }
        LogicalExpr::BinaryExpr { left, op, right } => LogicalExpr::BinaryExpr {
            left: boxed(left),
            op: *op,
            right: boxed(right),
        },
        LogicalExpr::UnaryExpr { op, expr } => LogicalExpr::UnaryExpr {
            op: *op,
            expr: boxed(expr),
        },
        LogicalExpr::ScalarFunction { fun, args } => LogicalExpr::ScalarFunction {
            fun: fun.clone(),
            args: args.iter().map(sub).collect(),
        },
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => LogicalExpr::Case {
            expr: expr.as_deref().map(boxed),
            when_then: when_then.iter().map(|(when, then)| (sub(when), sub(then))).collect(),
            else_result: else_result.as_deref().map(boxed),
        },
        LogicalExpr::Cast { expr, data_type } => LogicalExpr::Cast {
            expr: boxed(expr),
            data_type: data_type.clone(),
        },
        LogicalExpr::IsNull { expr, is_null } => LogicalExpr::IsNull {
            expr: boxed(expr),
            is_null: *is_null,
        },
        LogicalExpr::Between {
            expr,
            low,
            high,
            negated,
        } => LogicalExpr::Between {
            expr: boxed(expr),
            low: boxed(low),
            high: boxed(high),
            negated: *negated,
        },
        LogicalExpr::InList { expr, list, negated } => LogicalExpr::InList {
            expr: boxed(expr),
            list: list.iter().map(sub).collect(),
            negated: *negated,
        },
        LogicalExpr::InSet { expr, values, negated } => LogicalExpr::InSet {
            expr: boxed(expr),
            values: values.clone(),
            negated: *negated,
        },
        LogicalExpr::ArraySubscript { array, index } => LogicalExpr::ArraySubscript {
            array: boxed(array),
            index: boxed(index),
        },
        LogicalExpr::Tuple { items } => LogicalExpr::Tuple {
            items: items.iter().map(sub).collect(),
        },
        other => other.clone(),
    }
}

/// Functions the default-stability rule calls stable that are not immutable: the
/// clock and what reads the session.
fn is_stable_only(function: &str) -> bool {
    let lowered = function.to_ascii_lowercase();
    let name = lowered.strip_prefix("pg_catalog.").unwrap_or(&lowered);
    matches!(
        name,
        "current_timestamp"
            | "now"
            | "sysdate"
            | "getdate"
            | "systimestamp"
            | "sysdatetime"
            | "getutcdate"
            | "utc_timestamp"
            | "current_date"
            | "curdate"
            | "current_time"
            | "curtime"
            | "localtimestamp"
            | "localtime"
            | "unix_timestamp"
            | "current_user"
            | "session_user"
            | "current_role"
            | "user"
            | "current_schema"
            | "current_database"
            | "version"
            | "age"
    )
}

/// What PostgreSQL refuses in a generation expression, refused (42P17).
fn validate_expression(table: &str, column: &str, expr: &LogicalExpr) -> Result<()> {
    let each = |inner: &LogicalExpr| validate_expression(table, column, inner);
    match expr {
        LogicalExpr::Column { .. } | LogicalExpr::BoundColumn { .. } | LogicalExpr::Literal(_) => Ok(()),
        LogicalExpr::BinaryExpr { left, right, .. } => each(left).and_then(|()| each(right)),
        LogicalExpr::UnaryExpr { expr, .. }
        | LogicalExpr::Cast { expr, .. }
        | LogicalExpr::IsNull { expr, .. }
        | LogicalExpr::InSet { expr, .. } => each(expr),
        LogicalExpr::ScalarFunction { fun, args } => {
            let call = LogicalExpr::ScalarFunction {
                fun: fun.clone(),
                args: Vec::new(),
            };
            if is_stable_only(fun) || !crate::sql::default_volatility::default_is_stable(&call) {
                return Err(invalid(
                    table,
                    column,
                    format_args!("function {fun}(), which is not immutable,"),
                ));
            }
            args.iter().try_for_each(each)
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            if let Some(expr) = expr {
                each(expr)?;
            }
            for (when, then) in when_then {
                each(when)?;
                each(then)?;
            }
            else_result.as_deref().map_or(Ok(()), each)
        }
        LogicalExpr::Between { expr, low, high, .. } => each(expr).and_then(|()| each(low)).and_then(|()| each(high)),
        LogicalExpr::InList { expr, list, .. } => each(expr).and_then(|()| list.iter().try_for_each(each)),
        LogicalExpr::ArraySubscript { array, index } => each(array).and_then(|()| each(index)),
        LogicalExpr::Tuple { items } => items.iter().try_for_each(each),
        LogicalExpr::ScalarSubquery { .. } | LogicalExpr::InSubquery { .. } | LogicalExpr::Exists { .. } => {
            Err(invalid(table, column, "a subquery"))
        }
        LogicalExpr::AggregateFunction { .. } => Err(invalid(table, column, "an aggregate function")),
        LogicalExpr::WindowFunction { .. } => Err(invalid(table, column, "a window function")),
        LogicalExpr::Parameter { .. } => Err(invalid(table, column, "a parameter")),
        _ => Err(invalid(table, column, "this expression")),
    }
}

/// The columns a generation expression may read: each must exist in the table and
/// must not be generated itself (the column's own name included).
fn validate_references(
    table: &str,
    column: &str,
    expr: &LogicalExpr,
    is_generated: &dyn Fn(&str) -> Option<bool>,
) -> Result<()> {
    let mut read = Vec::new();
    columns_read(expr, &mut read);
    for name in read {
        match is_generated(&name) {
            None => {
                return Err(invalid(
                    table,
                    column,
                    format_args!("column \"{name}\", which the table does not have,"),
                ))
            }
            Some(true) => {
                return Err(invalid(
                    table,
                    column,
                    format_args!("a reference to the generated column \"{name}\""),
                ))
            }
            Some(false) => {}
        }
    }
    Ok(())
}

/// The marker a `VIRTUAL` generation expression is wrapped in by
/// [`rewrite_virtual`] (never evaluated).
const VIRTUAL_MARKER_FN: &str = "helios_generated_virtual";

/// The parser's grammar reads `GENERATED ALWAYS AS (expr) STORED` only: a
/// statement with `VIRTUAL`, or with neither keyword, fails to parse. Run after
/// that failure, this rewrites each such clause to `GENERATED ALWAYS AS
/// (<marker>(expr)) STORED`, which [`plan_generated`] refuses (0A000) in words
/// instead of a syntax error. `None` when the statement holds no such clause.
pub(crate) fn rewrite_virtual(sql: &str) -> Option<String> {
    let pieces = crate::sql::pieces::pieces(sql);
    let text = |index: usize| {
        pieces
            .get(index)
            .and_then(|piece| sql.get(piece.start..piece.end))
            .unwrap_or_default()
    };
    let word = |index: usize, keyword: &str| {
        pieces.get(index).is_some_and(|piece| piece.word) && text(index).eq_ignore_ascii_case(keyword)
    };
    // (open parenthesis end, close parenthesis start, end of a VIRTUAL keyword or of the parenthesis).
    let mut edits: Vec<(usize, usize, usize)> = Vec::new();
    for index in 0..pieces.len() {
        if !(word(index, "GENERATED") && word(index + 1, "ALWAYS") && word(index + 2, "AS") && text(index + 3) == "(") {
            continue;
        }
        let open = pieces.get(index + 3)?;
        let (close_index, close) = pieces.iter().enumerate().skip(index + 4).find(|(_, piece)| {
            piece.depth == open.depth && !piece.word && sql.get(piece.start..piece.end) == Some(")")
        })?;
        if word(close_index + 1, "STORED") {
            continue;
        }
        let end = if word(close_index + 1, "VIRTUAL") {
            pieces.get(close_index + 1)?.end
        } else {
            close.end
        };
        edits.push((open.end, close.start, end));
    }
    if edits.is_empty() {
        return None;
    }
    let mut rewritten = String::with_capacity(sql.len() + 64);
    let mut copied = 0usize;
    for (open_end, close_start, end) in edits {
        rewritten.push_str(sql.get(copied..open_end)?);
        rewritten.push_str(VIRTUAL_MARKER_FN);
        rewritten.push('(');
        rewritten.push_str(sql.get(open_end..close_start)?);
        rewritten.push_str(")) STORED");
        copied = end;
    }
    rewritten.push_str(sql.get(copied..)?);
    Some(rewritten)
}

/// `GENERATED ALWAYS AS (expr) [STORED | VIRTUAL]` of a column definition, as the
/// column's stored default. `VIRTUAL`, and the form with neither keyword
/// (PostgreSQL 18 reads it as `VIRTUAL`), are refused (0A000): a value computed on
/// every read needs every reader of the table to compute it.
pub(crate) fn plan_generated(
    planner: &crate::sql::planner::Planner<'_>,
    column: &sqlparser::ast::Ident,
    expr: &sqlparser::ast::Expr,
    mode: Option<&sqlparser::ast::GeneratedExpressionMode>,
) -> Result<LogicalExpr> {
    let marked_virtual = expr.to_string().trim_start().starts_with(VIRTUAL_MARKER_FN);
    if marked_virtual || !matches!(mode, Some(sqlparser::ast::GeneratedExpressionMode::Stored)) {
        return Err(Error::query_execution(format!(
            "a VIRTUAL generated column (\"{}\") {UNSUPPORTED_STATEMENT_KIND_MARKER}: declare it GENERATED ALWAYS AS \
             (…) STORED",
            column.value
        )));
    }
    Ok(wrap(planner.expr_to_logical(expr)?))
}

/// The CHECK every write of the table validates for one generated column:
/// `column IS NOT DISTINCT FROM CAST(expr AS type)`.
pub(crate) fn implicit_check(column: &str, data_type: &DataType, expr: &LogicalExpr) -> LogicalExpr {
    LogicalExpr::BinaryExpr {
        left: Box::new(LogicalExpr::Column {
            table: None,
            name: column.to_string(),
        }),
        op: BinaryOperator::IsNotDistinctFrom,
        right: Box::new(LogicalExpr::Cast {
            expr: Box::new(expr.clone()),
            data_type: data_type.clone(),
        }),
    }
}

/// The name of a generated column's CHECK.
pub(crate) fn implicit_check_name(table: &str, column: &str) -> String {
    let bare = table.rsplit('.').next().unwrap_or(table);
    format!("{bare}_{column}_generated")
}

/// Is this stored CHECK body (the catalog's JSON) the CHECK of a generated column
/// of `schema`? Catalog views and dumps leave it out: the column's definition
/// says it.
pub(crate) fn is_implicit_check(check_json: &str, schema: &Schema) -> bool {
    if !check_json.contains("IsNotDistinctFrom") {
        return false;
    }
    let Ok(LogicalExpr::BinaryExpr {
        left,
        op: BinaryOperator::IsNotDistinctFrom,
        right,
    }) = serde_json::from_str::<LogicalExpr>(check_json)
    else {
        return false;
    };
    let (LogicalExpr::Column { name, .. }, LogicalExpr::Cast { .. }) = (*left, *right) else {
        return false;
    };
    schema
        .columns
        .iter()
        .any(|column| column.name == name && is_generated_default(column.default_expr.as_deref()))
}

/// [`is_implicit_check`] for a catalog view that holds the table's name only.
pub(crate) fn is_implicit_check_of(storage: &crate::storage::StorageEngine, table: &str, check_json: &str) -> bool {
    check_json.contains("IsNotDistinctFrom")
        && storage
            .catalog()
            .get_table_schema(table)
            .is_ok_and(|schema| is_implicit_check(check_json, &schema))
}

/// `CREATE TABLE`: validate every generated column against the table's columns
/// and add its CHECK.
pub(crate) fn finish_create_table(
    table: &str,
    columns: &[ColumnDef],
    constraints: &mut Vec<TableConstraint>,
) -> Result<()> {
    for column in columns {
        let Some(expr) = column.default.as_ref().and_then(unwrap) else {
            continue;
        };
        if column.is_identity {
            return Err(invalid(table, &column.name, "an identity or serial column"));
        }
        validate_expression(table, &column.name, expr)?;
        validate_references(table, &column.name, expr, &|name| {
            columns
                .iter()
                .find(|other| other.name.eq_ignore_ascii_case(name))
                .map(|other| other.default.as_ref().and_then(unwrap).is_some())
        })?;
        constraints.push(TableConstraint::Check {
            name: Some(implicit_check_name(table, &column.name)),
            expression: implicit_check(&column.name, &column.data_type, expr),
        });
    }
    Ok(())
}

/// `ALTER TABLE … ADD COLUMN … GENERATED ALWAYS AS (expr) STORED`: validate the
/// expression against the table as the catalog has it, and return the clause
/// that adds the column's CHECK.
pub(crate) fn add_column_check(
    schema: Option<&Schema>,
    table: &str,
    column: &ColumnDef,
) -> Result<Option<LogicalPlan>> {
    let Some(expr) = column.default.as_ref().and_then(unwrap) else {
        return Ok(None);
    };
    if column.is_identity {
        return Err(invalid(table, &column.name, "an identity or serial column"));
    }
    validate_expression(table, &column.name, expr)?;
    if let Some(schema) = schema {
        validate_references(table, &column.name, expr, &|name| {
            if name.eq_ignore_ascii_case(&column.name) {
                return Some(true);
            }
            schema
                .columns
                .iter()
                .find(|other| other.name.eq_ignore_ascii_case(name))
                .map(|other| is_generated_default(other.default_expr.as_deref()))
        })?;
    }
    Ok(Some(LogicalPlan::AlterTableAddCheck {
        table_name: table.to_string(),
        constraint_name: Some(implicit_check_name(table, &column.name)),
        expression: implicit_check(&column.name, &column.data_type, expr),
    }))
}

/// What a column's stored default gives a row that names no value for it.
fn default_of(column: &Column) -> LogicalExpr {
    column
        .default_expr
        .as_deref()
        .and_then(|json| serde_json::from_str::<LogicalExpr>(json).ok())
        .unwrap_or(LogicalExpr::Literal(Value::Null))
}

/// `INSERT … VALUES`: the generated columns of `schema` computed for every row.
/// The column list comes back explicit, each generated column in it with its
/// generation expression over the row's own value expressions (a column the row
/// leaves out reads its default). A value given for a generated column, other
/// than `DEFAULT`, is refused (428C9). A table with no generated column comes
/// back as it was.
pub(crate) fn rewrite_insert(
    schema: &Schema,
    table: &str,
    columns: Option<Vec<String>>,
    rows: Vec<Vec<LogicalExpr>>,
) -> Result<(Option<Vec<String>>, Vec<Vec<LogicalExpr>>)> {
    let generated = generated_columns(schema);
    if generated.is_empty() {
        return Ok((columns, rows));
    }
    let width = rows.first().map_or(0, Vec::len);
    let listed: Vec<String> = match &columns {
        Some(columns) => columns.clone(),
        None => schema.columns.iter().take(width).map(|c| c.name.clone()).collect(),
    };
    if rows.iter().any(|row| row.len() != listed.len()) {
        // A row of another width: the executor's own report.
        return Ok((columns, rows));
    }
    let position_of = |name: &str| {
        listed
            .iter()
            .position(|listed| listed == name)
            .or_else(|| listed.iter().position(|listed| listed.eq_ignore_ascii_case(name)))
    };
    let mut out_columns = listed.clone();
    for (_, column, _) in &generated {
        if position_of(&column.name).is_none() {
            out_columns.push(column.name.clone());
        }
    }
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut out = row.clone();
        for (_, column, expr) in &generated {
            let given = position_of(&column.name);
            if let Some(value) = given.and_then(|position| row.get(position)) {
                if !matches!(value, LogicalExpr::DefaultValue) {
                    return Err(direct_write(table, &column.name));
                }
            }
            let mut unknown_key: Option<String> = None;
            let value = substitute(expr, &|name| {
                let source = schema
                    .columns
                    .iter()
                    .find(|c| c.name == name)
                    .or_else(|| schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)))?;
                match position_of(&source.name).and_then(|position| row.get(position)) {
                    Some(LogicalExpr::DefaultValue) | None => Some(default_of(source)),
                    Some(value) => Some(value.clone()),
                }
            });
            // A key the storage layer assigns (an omitted serial or identity
            // primary key) has no value at plan time.
            let mut read = Vec::new();
            columns_read(expr, &mut read);
            for name in read {
                let Some(source) = schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(&name)) else {
                    continue;
                };
                let left_out = !matches!(
                    position_of(&source.name).and_then(|position| row.get(position)),
                    Some(value) if !matches!(value, LogicalExpr::DefaultValue)
                );
                if left_out && source.primary_key && source.default_expr.is_none() {
                    unknown_key = Some(source.name.clone());
                }
            }
            if let Some(key) = unknown_key {
                return Err(Error::query_execution(format!(
                    "INSERT into \"{table}\" without a value for \"{key}\", which the generated column \"{}\" reads, \
                     {UNSUPPORTED_STATEMENT_KIND_MARKER}: give the key a value in the statement",
                    column.name
                )));
            }
            match given {
                Some(position) => {
                    if let Some(slot) = out.get_mut(position) {
                        *slot = value;
                    }
                }
                None => out.push(value),
            }
        }
        out_rows.push(out);
    }
    Ok((Some(out_columns), out_rows))
}

/// `UPDATE … SET` / `ON CONFLICT DO UPDATE SET`: an assignment of every generated
/// column that reads an assigned column, with its generation expression over the
/// assigned values (a column that is not assigned reads the row). An assignment
/// to a generated column, other than `DEFAULT`, is refused (428C9).
pub(crate) fn rewrite_assignments(
    schema: &Schema,
    table: &str,
    assignments: Vec<(String, LogicalExpr)>,
) -> Result<Vec<(String, LogicalExpr)>> {
    let generated = generated_columns(schema);
    if generated.is_empty() {
        return Ok(assignments);
    }
    let bare = |target: &str| target.rsplit('.').next().unwrap_or(target).to_string();
    let mut kept: Vec<(String, LogicalExpr)> = Vec::with_capacity(assignments.len());
    let mut recompute: Vec<String> = Vec::new();
    for (target, value) in assignments {
        let name = bare(&target);
        if let Some((_, column, _)) = generated
            .iter()
            .find(|(_, column, _)| column.name.eq_ignore_ascii_case(&name))
        {
            // `SET g = DEFAULT` arrives as the column's stored default.
            if unwrap(&value).is_none() && !matches!(value, LogicalExpr::DefaultValue) {
                return Err(direct_write(table, &column.name));
            }
            recompute.push(column.name.clone());
            continue;
        }
        kept.push((target, value));
    }
    let assigned: Vec<(String, LogicalExpr)> = kept
        .iter()
        .map(|(target, value)| (bare(target), value.clone()))
        .collect();
    for (_, column, expr) in &generated {
        let mut read = Vec::new();
        columns_read(expr, &mut read);
        let touched = recompute.contains(&column.name)
            || read
                .iter()
                .any(|name| assigned.iter().any(|(target, _)| target.eq_ignore_ascii_case(name)));
        if !touched {
            continue;
        }
        let value = substitute(expr, &|name| {
            assigned
                .iter()
                .find(|(target, _)| target.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        });
        kept.push((column.name.clone(), value));
    }
    Ok(kept)
}

/// `INSERT … SELECT`: a projection over the source that adds the generated
/// columns, each computed from the source's own output columns by name. Refused
/// (0A000) when the source's output names cannot be told apart, and (428C9) when
/// the statement gives a generated column a value.
fn rewrite_insert_select(
    schema: &Schema,
    table: &str,
    columns: Option<Vec<String>>,
    source: LogicalPlan,
) -> Result<(Option<Vec<String>>, LogicalPlan)> {
    let generated = generated_columns(schema);
    if generated.is_empty() {
        return Ok((columns, source));
    }
    let source_names: Vec<String> = source.schema().columns.iter().map(|c| c.name.clone()).collect();
    let listed: Vec<String> = match &columns {
        Some(columns) => columns.clone(),
        None => schema
            .columns
            .iter()
            .take(source_names.len())
            .map(|c| c.name.clone())
            .collect(),
    };
    for (_, column, _) in &generated {
        if listed.iter().any(|name| name.eq_ignore_ascii_case(&column.name)) {
            return Err(direct_write(table, &column.name));
        }
    }
    let distinct = source_names.iter().enumerate().all(|(index, name)| {
        !name.is_empty()
            && name != "?column?"
            && !source_names
                .iter()
                .skip(index + 1)
                .any(|other| other.eq_ignore_ascii_case(name))
    });
    if !distinct || listed.len() != source_names.len() {
        return Err(Error::query_execution(format!(
            "INSERT … SELECT into \"{table}\", which has a generated column, from a query whose output columns are \
             not all named differently {UNSUPPORTED_STATEMENT_KIND_MARKER}: give each output column its own alias"
        )));
    }
    let reference = |name: &str| LogicalExpr::Column {
        table: None,
        name: name.to_string(),
    };
    let mut exprs: Vec<LogicalExpr> = source_names.iter().map(|name| reference(name)).collect();
    let mut aliases = source_names.clone();
    let mut out_columns = listed.clone();
    for (_, column, expr) in &generated {
        let value = substitute(expr, &|name| {
            let source_column = schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name))?;
            match listed
                .iter()
                .position(|listed| listed.eq_ignore_ascii_case(&source_column.name))
                .and_then(|position| source_names.get(position))
            {
                Some(output) => Some(reference(output)),
                None => Some(default_of(source_column)),
            }
        });
        exprs.push(value);
        aliases.push(column.name.clone());
        out_columns.push(column.name.clone());
    }
    Ok((
        Some(out_columns),
        LogicalPlan::Project {
            input: Box::new(source),
            exprs,
            aliases,
            distinct: false,
            distinct_on: None,
            source_alias: None,
        },
    ))
}

/// The planner's INSERT, with the target's generated columns computed (see the
/// module docs). Any other plan, and an INSERT into a table with no generated
/// column, comes back as it was.
pub(crate) fn finish_insert_plan(
    catalog: Option<&crate::storage::Catalog<'_>>,
    plan: LogicalPlan,
) -> Result<LogicalPlan> {
    let table = match &plan {
        LogicalPlan::Insert { table_name, .. } | LogicalPlan::InsertSelect { table_name, .. } => table_name.clone(),
        _ => return Ok(plan),
    };
    let Some(schema) = catalog.and_then(|catalog| catalog.get_table_schema(&table).ok()) else {
        return Ok(plan);
    };
    if !schema
        .columns
        .iter()
        .any(|column| is_generated_default(column.default_expr.as_deref()))
    {
        return Ok(plan);
    }
    let finish_conflict = |on_conflict: Option<crate::sql::logical_plan::OnConflictAction>| match on_conflict {
        Some(crate::sql::logical_plan::OnConflictAction::DoUpdate {
            assignments,
            selection,
            conflict_target,
        }) => Ok::<_, Error>(Some(crate::sql::logical_plan::OnConflictAction::DoUpdate {
            assignments: rewrite_assignments(&schema, &table, assignments)?,
            selection,
            conflict_target,
        })),
        other => Ok(other),
    };
    match plan {
        LogicalPlan::Insert {
            table_name,
            columns,
            values,
            returning,
            on_conflict,
        } => {
            let (columns, values) = rewrite_insert(&schema, &table, columns, values)?;
            Ok(LogicalPlan::Insert {
                table_name,
                columns,
                values,
                returning,
                on_conflict: finish_conflict(on_conflict)?,
            })
        }
        LogicalPlan::InsertSelect {
            table_name,
            columns,
            source,
            returning,
            on_conflict,
        } => {
            let (columns, source) = rewrite_insert_select(&schema, &table, columns, *source)?;
            Ok(LogicalPlan::InsertSelect {
                table_name,
                columns,
                source: Box::new(source),
                returning,
                on_conflict: finish_conflict(on_conflict)?,
            })
        }
        other => Ok(other),
    }
}

/// The planner's `UPDATE … SET` list, with the target's generated columns
/// assigned (see [`rewrite_assignments`]).
pub(crate) fn finish_update_assignments(
    catalog: Option<&crate::storage::Catalog<'_>>,
    table: &str,
    assignments: Vec<(String, LogicalExpr)>,
) -> Result<Vec<(String, LogicalExpr)>> {
    match catalog.and_then(|catalog| catalog.get_table_schema(table).ok()) {
        Some(schema) => rewrite_assignments(&schema, table, assignments),
        None => Ok(assignments),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn column(name: &str) -> LogicalExpr {
        LogicalExpr::Column {
            table: None,
            name: name.to_string(),
        }
    }

    fn sum() -> LogicalExpr {
        LogicalExpr::BinaryExpr {
            left: Box::new(column("a")),
            op: BinaryOperator::Plus,
            right: Box::new(column("b")),
        }
    }

    fn schema() -> Schema {
        let mut g = Column::new("g", DataType::Int4);
        g.default_expr = Some(serde_json::to_string(&wrap(sum())).unwrap());
        Schema::new(vec![
            Column::new("a", DataType::Int4),
            Column::new("b", DataType::Int4),
            g,
        ])
    }

    fn literal(value: i32) -> LogicalExpr {
        LogicalExpr::Literal(Value::Int4(value))
    }

    #[test]
    fn an_insert_gets_the_column_computed_from_its_own_values() {
        let (columns, rows) = rewrite_insert(
            &schema(),
            "t",
            Some(vec!["b".into(), "a".into()]),
            vec![vec![literal(2), literal(1)]],
        )
        .unwrap();
        assert_eq!(columns.unwrap(), vec!["b", "a", "g"]);
        assert_eq!(
            rows[0][2],
            LogicalExpr::BinaryExpr {
                left: Box::new(literal(1)),
                op: BinaryOperator::Plus,
                right: Box::new(literal(2)),
            }
        );
        let refused = rewrite_insert(&schema(), "t", None, vec![vec![literal(1), literal(2), literal(9)]]).unwrap_err();
        assert!(refused.to_string().contains(GENERATED_COLUMN_WRITE_MARKER));
        let (_, rows) = rewrite_insert(
            &schema(),
            "t",
            None,
            vec![vec![literal(1), literal(2), LogicalExpr::DefaultValue]],
        )
        .unwrap();
        assert_eq!(rows[0].len(), 3);
        assert!(matches!(rows[0][2], LogicalExpr::BinaryExpr { .. }));
    }

    #[test]
    fn an_update_of_a_column_it_reads_assigns_it_too() {
        let assignments = rewrite_assignments(&schema(), "t", vec![("a".into(), literal(5))]).unwrap();
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[1].0, "g");
        assert_eq!(
            assignments[1].1,
            LogicalExpr::BinaryExpr {
                left: Box::new(literal(5)),
                op: BinaryOperator::Plus,
                right: Box::new(column("b")),
            }
        );
        let refused = rewrite_assignments(&schema(), "t", vec![("g".into(), literal(5))]).unwrap_err();
        assert!(refused.to_string().contains(GENERATED_COLUMN_WRITE_MARKER));
    }

    #[test]
    fn a_virtual_clause_is_marked_and_a_stored_one_left() {
        assert_eq!(
            rewrite_virtual("CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + (1)) VIRTUAL, c INT)").unwrap(),
            "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (helios_generated_virtual(a + (1))) STORED, c INT)"
        );
        assert_eq!(
            rewrite_virtual("ALTER TABLE t ADD COLUMN b INT GENERATED ALWAYS AS (a * 2)").unwrap(),
            "ALTER TABLE t ADD COLUMN b INT GENERATED ALWAYS AS (helios_generated_virtual(a * 2)) STORED"
        );
        assert!(rewrite_virtual("CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED)").is_none());
        assert!(rewrite_virtual("CREATE TABLE t (id INT GENERATED ALWAYS AS IDENTITY)").is_none());
    }

    #[test]
    fn the_columns_check_is_recognised_and_other_checks_are_not() {
        let check = serde_json::to_string(&implicit_check("g", &DataType::Int4, &sum())).unwrap();
        assert!(is_implicit_check(&check, &schema()));
        let other = serde_json::to_string(&LogicalExpr::BinaryExpr {
            left: Box::new(column("a")),
            op: BinaryOperator::IsNotDistinctFrom,
            right: Box::new(column("b")),
        })
        .unwrap();
        assert!(!is_implicit_check(&other, &schema()));
    }
}
