//! PL/pgSQL the PostgreSQL way: variables written as bare names and bound as
//! parameters, `FOUND` and the row count, records, `RAISE`, and `EXCEPTION`
//! blocks.
//!
//! Bare names. Before a statement or an expression of a routine goes to the
//! engine, every identifier in an expression position that names a variable
//! in scope is replaced by a parameter (`$1`, `$2`, …) and the variable's
//! value is bound to it. The value never becomes part of the statement text.
//! A name that is both a variable and a column of a table the statement reads
//! is ambiguous (SQLSTATE 42702), as in PostgreSQL — except in a statement
//! that also uses the older `$name` spelling, where a bare name stays the
//! column it always was. `label.name` and `routine.name` always mean the
//! variable, `record.field` the field of a record variable.

use super::{ExecutionContext, ProceduralExecutor, Variable};
use crate::sql::procedural::ast::{ExceptionCondition, ExceptionHandler, PlExpr, RaiseLevel};
use crate::sql::procedural::conditions::{condition_sqlstate, sqlstate_matches};
use crate::sql::procedural::host_channel;
use crate::sql::sql_scan::{self, TokKind};
use crate::{Error, Result, Value};
use sqlparser::ast::{self, Expr};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

/// The prefix of the savepoints an `EXCEPTION` block works under. The engine
/// running a routine's statements answers these three statement texts itself.
pub(crate) const BLOCK_SAVEPOINT_PREFIX: &str = "__hdb_plpgsql_block_";

/// What a statement run through the host did.
pub(super) struct SqlOutcome {
    pub rows: Vec<Vec<Value>>,
    /// The names of the returned columns, when the host reported them.
    pub columns: Vec<String>,
    pub row_count: u64,
}

/// A row held by a record variable.
#[derive(Debug, Clone)]
pub(super) struct Record {
    columns: Vec<String>,
    values: Vec<Value>,
}

/// The error an `EXCEPTION` handler is handling.
#[derive(Debug, Clone)]
pub(super) struct Caught {
    sqlstate: String,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
}

/// The state this module keeps in an [`ExecutionContext`].
#[derive(Default)]
pub(crate) struct PgState {
    /// The routine's name, for `routine.variable`.
    pub routine_name: Option<String>,
    labels: Vec<String>,
    row_count: u64,
    /// Statements that may have changed data, counted.
    writes: u64,
    records: HashMap<String, Record>,
    handling: Vec<Caught>,
    table_columns: HashMap<String, Vec<String>>,
    savepoints: u64,
}

fn unsupported(message: impl Into<String>) -> Error {
    Error::sql_state("0A000", message)
}

/// A value as `RAISE` and string contexts show it.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "<NULL>".to_string(),
        other => other.to_string(),
    }
}

/// The SQLSTATE an error reaches a client with.
pub(super) fn sqlstate_of(error: &Error) -> String {
    crate::protocol::postgres::handler::sqlstate_for_error(error).to_string()
}

/// The declared spelling of the variable an unquoted `name` means.
pub(super) fn declared_name(ctx: &ExecutionContext<'_>, name: &str) -> Option<String> {
    ctx.scope.find(name, false).map(|(declared, _)| declared.to_string())
}

/// `FOUND` exists from the start of a routine and is false until a statement sets it.
pub(super) fn begin_routine(ctx: &mut ExecutionContext<'_>) {
    if ctx.scope.find("found", true).is_none() {
        let _ = ctx.scope.declare(
            "found".to_string(),
            Variable {
                value: Value::Boolean(false),
                data_type: Some(crate::DataType::Boolean),
                is_constant: false,
                not_null: false,
            },
        );
    }
}

pub(super) fn set_found(ctx: &mut ExecutionContext<'_>, found: bool) {
    if let Some(name) = declared_name(ctx, "found") {
        let _ = ctx.scope.set(&name, Value::Boolean(found));
    }
}

pub(super) fn set_row_count(ctx: &mut ExecutionContext<'_>, count: u64) {
    ctx.pg.row_count = count;
}

pub(super) fn row_count(ctx: &ExecutionContext<'_>) -> Value {
    Value::Int8(i64::try_from(ctx.pg.row_count).unwrap_or(i64::MAX))
}

pub(super) fn enter_label(ctx: &mut ExecutionContext<'_>, label: Option<&str>) -> bool {
    match label {
        Some(label) => {
            ctx.pg.labels.push(label.to_ascii_lowercase());
            true
        }
        None => false,
    }
}

pub(super) fn leave_label(ctx: &mut ExecutionContext<'_>, entered: bool) {
    if entered {
        ctx.pg.labels.pop();
    }
}

/// A variable was declared or given a plain value: it holds no row any more.
pub(super) fn forget_record(ctx: &mut ExecutionContext<'_>, name: &str) {
    ctx.pg.records.remove(name);
}

/// Hand one statement text to the host with `params` bound to its `$n`.
fn run_host(ctx: &mut ExecutionContext<'_>, sql: &str, params: Vec<Value>) -> Result<SqlOutcome> {
    host_channel::clear();
    let bound = !params.is_empty();
    if bound {
        host_channel::offer_params(params);
    }
    let result = (ctx.sql_executor)(sql);
    let untaken = host_channel::params_pending();
    let report = host_channel::take_report();
    host_channel::clear();
    let rows = result?;
    if bound && untaken {
        return Err(unsupported(
            "this execution path cannot bind a routine's variables to its statements; \
             write them with the `$` sigil here",
        ));
    }
    let (row_count, columns) = match report {
        Some(report) => (report.row_count, report.columns),
        None => (rows.len() as u64, Vec::new()),
    };
    Ok(SqlOutcome {
        rows,
        columns,
        row_count,
    })
}

/// Does the statement read only (as far as its first word says)?
fn reads_only(sql: &str) -> bool {
    let toks = sql_scan::tokens(sql);
    toks.first()
        .is_some_and(|t| t.is_word(sql, "SELECT") || t.is_word(sql, "VALUES") || t.is_word(sql, "TABLE"))
}

/// Run a statement of the routine: `$name` the older way, then bare names
/// bound as parameters.
pub(super) fn run_sql(ctx: &mut ExecutionContext<'_>, sql: &str) -> Result<SqlOutcome> {
    let interpolated = ctx.interpolate(sql);
    let uses_sigil = interpolated != sql;
    let (text, params) = bind_bare_names(ctx, &interpolated, uses_sigil)?;
    if !reads_only(&text) {
        ctx.pg.writes += 1;
    }
    run_host(ctx, &text, params)
}

/// Run a computed command (`EXECUTE`) with its USING values. Nothing of the
/// routine is substituted into it: its `$n` are the USING values only.
pub(super) fn run_dynamic(ctx: &mut ExecutionContext<'_>, sql: &str, params: Vec<Value>) -> Result<SqlOutcome> {
    ctx.pg.writes += 1;
    run_host(ctx, sql, params)
}

/// The column names of a table the statement names, asked of the engine once
/// per routine call.
fn table_columns(ctx: &mut ExecutionContext<'_>, table: &str) -> Vec<String> {
    if let Some(known) = ctx.pg.table_columns.get(table) {
        return known.clone();
    }
    let sql = format!(
        "SELECT column_name FROM information_schema.columns WHERE table_name = '{}'",
        table.replace('\'', "''")
    );
    host_channel::clear();
    let rows = (ctx.sql_executor)(&sql).unwrap_or_default();
    host_channel::clear();
    let columns: Vec<String> = rows
        .into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(Value::String(name)) => Some(name.to_lowercase()),
            _ => None,
        })
        .collect();
    ctx.pg.table_columns.insert(table.to_string(), columns.clone());
    columns
}

fn ident_name(ident: &ast::Ident) -> (String, bool) {
    match ident.quote_style {
        Some(_) => (ident.value.clone(), true),
        None => (ident.value.to_lowercase(), false),
    }
}

/// What the statement's own names are: its relations, their aliases, and the
/// aliases of its select lists.
#[derive(Default)]
struct StatementNames {
    relations: Vec<String>,
    qualifiers: HashSet<String>,
    select_aliases: HashSet<String>,
    has_placeholder: bool,
}

impl StatementNames {
    fn note_set_expr(&mut self, body: &ast::SetExpr) {
        match body {
            ast::SetExpr::Select(select) => {
                for item in &select.projection {
                    if let ast::SelectItem::ExprWithAlias { alias, .. } = item {
                        self.select_aliases.insert(ident_name(alias).0);
                    }
                }
            }
            ast::SetExpr::SetOperation { left, right, .. } => {
                self.note_set_expr(left);
                self.note_set_expr(right);
            }
            _ => {}
        }
    }
}

impl ast::Visitor for StatementNames {
    type Break = ();

    fn pre_visit_relation(&mut self, relation: &ast::ObjectName) -> ControlFlow<()> {
        if let Some(last) = relation.0.last() {
            let name = ident_name(last).0;
            self.qualifiers.insert(name.clone());
            self.relations.push(name);
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &ast::TableFactor) -> ControlFlow<()> {
        let alias = match factor {
            ast::TableFactor::Table { alias, .. } | ast::TableFactor::Derived { alias, .. } => alias.as_ref(),
            _ => None,
        };
        if let Some(alias) = alias {
            self.qualifiers.insert(ident_name(&alias.name).0);
            for column in &alias.columns {
                self.select_aliases.insert(column.to_string().to_lowercase());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_query(&mut self, query: &ast::Query) -> ControlFlow<()> {
        self.note_set_expr(&query.body);
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                self.qualifiers.insert(ident_name(&cte.alias.name).0);
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if matches!(expr, Expr::Value(ast::Value::Placeholder(_))) {
            self.has_placeholder = true;
        }
        ControlFlow::Continue(())
    }
}

/// The target of an `INSERT INTO target …` whose source cannot see the
/// target's columns (no `ON CONFLICT`, no `RETURNING`).
fn insert_target_out_of_scope(sql: &str) -> Option<String> {
    let toks = sql_scan::tokens(sql);
    if !toks.first()?.is_word(sql, "INSERT") || !toks.get(1)?.is_word(sql, "INTO") {
        return None;
    }
    if toks
        .iter()
        .any(|t| t.depth == 0 && (t.is_word(sql, "RETURNING") || t.is_word(sql, "CONFLICT")))
    {
        return None;
    }
    let mut name = toks.get(2)?;
    let mut at = 3;
    while toks.get(at).is_some_and(|t| t.is_punct(sql, '.')) {
        name = toks.get(at + 1)?;
        at += 2;
    }
    matches!(name.kind, TokKind::Word | TokKind::Quoted).then(|| sql_scan::ident_value(name, sql))
}

/// Replace the variables a statement names by parameters. Returns the text to
/// run and the values of its `$1…`; the text is returned unchanged when it
/// names no variable.
fn bind_bare_names(ctx: &mut ExecutionContext<'_>, sql: &str, column_first: bool) -> Result<(String, Vec<Value>)> {
    let unchanged = || Ok((sql.to_string(), Vec::new()));
    // The common case, decided without parsing: no word of the statement is a
    // variable, a label or the routine's name.
    let toks = sql_scan::tokens(sql);
    let names_something = toks.iter().any(|tok| match tok.kind {
        TokKind::Word => {
            let word = tok.text(sql).to_lowercase();
            ctx.scope.find(&word, false).is_some()
                || ctx.pg.labels.contains(&word)
                || ctx.pg.routine_name.as_deref() == Some(word.as_str())
        }
        TokKind::Quoted => ctx.scope.find(&sql_scan::ident_value(tok, sql), true).is_some(),
        _ => false,
    });
    if !names_something {
        return unchanged();
    }
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let Ok(mut statements) = sqlparser::parser::Parser::parse_sql(&dialect, sql) else {
        // Not this parser's grammar: the engine reads it as written.
        return unchanged();
    };
    let mut statement = match (statements.pop(), statements.is_empty()) {
        (Some(statement), true) => statement,
        _ => return unchanged(),
    };

    let mut names = StatementNames::default();
    {
        use ast::Visit;
        let _ = statement.visit(&mut names);
    }
    if names.has_placeholder {
        // The statement has parameters of its own; theirs and ours cannot be told apart.
        return unchanged();
    }
    let mut columns: HashSet<String> = names.select_aliases.clone();
    let hidden_target = insert_target_out_of_scope(sql);
    for relation in &names.relations {
        let seen = names.relations.iter().filter(|r| *r == relation).count();
        if hidden_target.as_deref() == Some(relation.as_str()) && seen == 1 {
            continue;
        }
        columns.extend(table_columns(ctx, relation));
    }

    let mut params: Vec<Value> = Vec::new();
    let mut slots: HashMap<String, usize> = HashMap::new();
    let mut problem: Option<Error> = None;
    let mut bind = |key: String, value: Value, params: &mut Vec<Value>| -> Expr {
        let slot = *slots.entry(key).or_insert_with(|| {
            params.push(value);
            params.len()
        });
        Expr::Value(ast::Value::Placeholder(format!("${slot}")))
    };
    let _ = ast::visit_expressions_mut(&mut statement, |expr: &mut Expr| {
        if problem.is_some() {
            return ControlFlow::<()>::Continue(());
        }
        let replacement = match expr {
            Expr::Identifier(ident) => {
                let (name, quoted) = ident_name(ident);
                match ctx.scope.find(&name, quoted) {
                    None => None,
                    Some((declared, variable)) => {
                        if columns.contains(&name.to_lowercase()) {
                            if !column_first {
                                problem = Some(Error::Raised {
                                    sqlstate: "42702".to_string(),
                                    message: format!("column reference \"{name}\" is ambiguous"),
                                    detail: Some(
                                        "It could refer to either a PL/pgSQL variable or a table column.".to_string(),
                                    ),
                                    hint: Some(
                                        "Qualify the column with its table, or the variable with its block label \
                                         or the routine's name."
                                            .to_string(),
                                    ),
                                });
                            }
                            None
                        } else if ctx.pg.records.get(declared).is_some_and(|r| r.columns.len() != 1) {
                            problem = Some(unsupported(format!(
                                "record variable \"{name}\" cannot be used as one value; name its fields"
                            )));
                            None
                        } else {
                            Some(bind(declared.to_string(), variable.value.clone(), &mut params))
                        }
                    }
                }
            }
            Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [first, second] => {
                    let (qualifier, quoted) = ident_name(first);
                    let (member, member_quoted) = ident_name(second);
                    let is_scope_name = !quoted
                        && (ctx.pg.labels.contains(&qualifier)
                            || ctx.pg.routine_name.as_deref().map(str::to_lowercase).as_deref()
                                == Some(qualifier.as_str()));
                    if is_scope_name && !names.qualifiers.contains(&qualifier) {
                        match ctx.scope.find(&member, member_quoted) {
                            Some((declared, variable)) => {
                                Some(bind(declared.to_string(), variable.value.clone(), &mut params))
                            }
                            None => {
                                problem = Some(Error::sql_state(
                                    "42703",
                                    format!("\"{qualifier}.{member}\" is not a known variable"),
                                ));
                                None
                            }
                        }
                    } else {
                        let record = ctx
                            .scope
                            .find(&qualifier, quoted)
                            .and_then(|(declared, _)| ctx.pg.records.get(declared).map(|r| (declared, r)));
                        match record {
                            Some(_) if names.qualifiers.contains(&qualifier) => {
                                problem = Some(Error::sql_state(
                                    "42702",
                                    format!(
                                        "\"{qualifier}\" is both a record variable and a table of the statement"
                                    ),
                                ));
                                None
                            }
                            Some((declared, record)) => {
                                let field = record
                                    .columns
                                    .iter()
                                    .position(|c| c.eq_ignore_ascii_case(&member))
                                    .and_then(|i| record.values.get(i));
                                match field {
                                    Some(value) => {
                                        Some(bind(format!("{declared}\u{0}{member}"), value.clone(), &mut params))
                                    }
                                    None => {
                                        problem = Some(Error::sql_state(
                                            "42703",
                                            format!("record \"{qualifier}\" has no field \"{member}\""),
                                        ));
                                        None
                                    }
                                }
                            }
                            None => None,
                        }
                    }
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(replacement) = replacement {
            *expr = replacement;
        }
        ControlFlow::Continue(())
    });
    if let Some(problem) = problem {
        return Err(problem);
    }
    if params.is_empty() {
        return unchanged();
    }
    Ok((statement.to_string(), params))
}

/// Put the first row of a result into the INTO targets.
pub(super) fn assign_into(
    ctx: &mut ExecutionContext<'_>,
    targets: &[String],
    outcome: SqlOutcome,
    strict: bool,
) -> Result<bool> {
    if strict {
        match outcome.rows.len() {
            0 => return Err(Error::sql_state("P0002", "query returned no rows")),
            1 => {}
            _ => return Err(Error::sql_state("P0003", "query returned more than one row")),
        }
    }
    let SqlOutcome { rows, columns, .. } = outcome;
    let row = rows.into_iter().next();
    let found = row.is_some();
    if targets.is_empty() {
        return Ok(found);
    }
    let mut names = Vec::with_capacity(targets.len());
    for target in targets {
        names.push(
            declared_name(ctx, target)
                .ok_or_else(|| Error::sql_state("42601", format!("\"{target}\" is not a known variable")))?,
        );
    }
    let width = row.as_ref().map_or(columns.len(), Vec::len);
    // One untyped target takes the whole row: it is a record.
    if let [only] = names.as_slice() {
        let untyped = ctx.scope.find(only, true).is_some_and(|(_, v)| v.data_type.is_none());
        if untyped && (width > 1 || !columns.is_empty()) {
            let values = row.unwrap_or_else(|| vec![Value::Null; width]);
            let first = values.first().cloned().unwrap_or(Value::Null);
            if columns.len() == values.len() {
                ctx.pg.records.insert(only.clone(), Record { columns, values });
            } else {
                ctx.pg.records.remove(only);
            }
            ctx.scope.set(only, first)?;
            return Ok(found);
        }
    }
    if row.as_ref().is_some_and(|r| r.len() < names.len()) {
        return Err(Error::sql_state(
            "42601",
            format!("INTO has {} targets but the query returns {width} column(s)", names.len()),
        ));
    }
    let mut values = row.unwrap_or_default().into_iter();
    for name in &names {
        let value = values.next().unwrap_or(Value::Null);
        let declared = ctx.scope.find(name, true).and_then(|(_, v)| v.data_type.clone());
        let value = ProceduralExecutor::cast_to_declared(ctx, value, declared.as_ref(), name)?;
        ctx.pg.records.remove(name);
        ctx.scope.set(name, value)?;
    }
    Ok(found)
}

/// Give a loop variable the current row of its query.
pub(super) fn set_loop_row(
    ctx: &mut ExecutionContext<'_>,
    variable: &str,
    columns: &[String],
    row: Vec<Value>,
    typed: Option<&crate::DataType>,
) -> Result<()> {
    let first = row.first().cloned().unwrap_or(Value::Null);
    let first = ProceduralExecutor::cast_to_declared(ctx, first, typed, variable)?;
    if typed.is_none() && columns.len() == row.len() && !columns.is_empty() {
        ctx.pg.records.insert(
            variable.to_string(),
            Record {
                columns: columns.to_vec(),
                values: row,
            },
        );
    } else {
        ctx.pg.records.remove(variable);
    }
    ctx.scope.set(variable, first)
}

/// `RAISE`.
pub(super) fn raise(
    ctx: &mut ExecutionContext<'_>,
    level: RaiseLevel,
    condition: Option<&str>,
    format: Option<&str>,
    args: &[PlExpr],
    options: &[(String, PlExpr)],
) -> Result<()> {
    if condition.is_none() && format.is_none() && options.is_empty() {
        // A bare RAISE: the error being handled, again.
        return match ctx.pg.handling.last() {
            Some(caught) => Err(Error::Raised {
                sqlstate: caught.sqlstate.clone(),
                message: caught.message.clone(),
                detail: caught.detail.clone(),
                hint: caught.hint.clone(),
            }),
            None => Err(Error::sql_state(
                "0Z002",
                "RAISE without parameters cannot be used outside an exception handler",
            )),
        };
    }
    let mut message = match format {
        Some(format) => {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(ProceduralExecutor::eval_expr(arg, ctx, "RAISE argument")?);
            }
            let mut values = values.iter();
            let mut out = String::with_capacity(format.len());
            let mut chars = format.chars().peekable();
            while let Some(c) = chars.next() {
                if c != '%' {
                    out.push(c);
                } else if chars.peek() == Some(&'%') {
                    chars.next();
                    out.push('%');
                } else {
                    match values.next() {
                        Some(value) => out.push_str(&value_text(value)),
                        None => return Err(Error::sql_state("42601", "too few parameters specified for RAISE")),
                    }
                }
            }
            if values.next().is_some() {
                return Err(Error::sql_state("42601", "too many parameters specified for RAISE"));
            }
            Some(out)
        }
        None => None,
    };
    let mut sqlstate = condition.map(str::to_string);
    let mut detail = None;
    let mut hint = None;
    for (name, expr) in options {
        let value = ProceduralExecutor::eval_expr(expr, ctx, "RAISE option")?;
        if matches!(value, Value::Null) {
            return Err(Error::sql_state(
                "22004",
                format!("RAISE statement option {name} cannot be null"),
            ));
        }
        let text = value_text(&value);
        match name.as_str() {
            "MESSAGE" => message = Some(text),
            "DETAIL" => detail = Some(text),
            "HINT" => hint = Some(text),
            _ => {
                let code = match condition_sqlstate(&text.to_ascii_lowercase()) {
                    Some(code) => code.to_string(),
                    None if text.len() == 5
                        && text.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()) =>
                    {
                        text
                    }
                    None => {
                        return Err(Error::sql_state(
                            "42704",
                            format!("unrecognized exception condition \"{text}\""),
                        ))
                    }
                };
                sqlstate = Some(code);
            }
        }
    }
    let is_exception = matches!(level, RaiseLevel::Exception);
    let sqlstate = sqlstate.unwrap_or_else(|| {
        match level {
            RaiseLevel::Exception => "P0001",
            RaiseLevel::Warning => "01000",
            _ => "00000",
        }
        .to_string()
    });
    let message = message.unwrap_or_else(|| sqlstate.clone());
    if is_exception {
        return Err(Error::Raised {
            sqlstate,
            message,
            detail,
            hint,
        });
    }
    let severity = match level {
        RaiseLevel::Warning => "WARNING",
        RaiseLevel::Notice => "NOTICE",
        RaiseLevel::Info => "INFO",
        RaiseLevel::Log => "LOG",
        RaiseLevel::Debug | RaiseLevel::Exception => "DEBUG",
    };
    tracing::debug!("RAISE {severity}: {message}");
    // WARNING, NOTICE and INFO reach the client; LOG and DEBUG stay in the server log.
    if matches!(level, RaiseLevel::Warning | RaiseLevel::Notice | RaiseLevel::Info) {
        if let Some(session) = crate::session_scoped_state_tls() {
            session.push_notice(crate::SessionNotice {
                severity: severity.to_string(),
                code: sqlstate,
                message,
            });
        }
    }
    Ok(())
}

/// The handler that takes `error`, if one does.
pub(super) fn matching_handler<'h>(handlers: &'h [ExceptionHandler], error: &Error) -> Option<&'h ExceptionHandler> {
    let sqlstate = sqlstate_of(error);
    handlers.iter().find(|handler| {
        handler.conditions.iter().any(|condition| match condition {
            // A cancelled query and a failed assertion are not for OTHERS.
            ExceptionCondition::Others => !matches!(sqlstate.as_str(), "57014" | "P0004"),
            ExceptionCondition::SqlState(code) => sqlstate_matches(code, &sqlstate),
            ExceptionCondition::Named(name) => {
                condition_sqlstate(&name.to_ascii_lowercase()).is_some_and(|code| sqlstate_matches(code, &sqlstate))
            }
        })
    })
}

/// Where a block with an `EXCEPTION` clause started.
pub(super) struct BlockMark {
    savepoint: Option<String>,
    writes: u64,
}

/// Mark the start of a block that has handlers.
pub(super) fn mark_block(ctx: &mut ExecutionContext<'_>) -> BlockMark {
    ctx.pg.savepoints += 1;
    let name = format!("{BLOCK_SAVEPOINT_PREFIX}{}", ctx.pg.savepoints);
    host_channel::clear();
    let made = (ctx.sql_executor)(&format!("SAVEPOINT {name}")).is_ok();
    host_channel::clear();
    BlockMark {
        savepoint: made.then_some(name),
        writes: ctx.pg.writes,
    }
}

/// The block ended without an error a handler took.
pub(super) fn release_block(ctx: &mut ExecutionContext<'_>, mark: BlockMark) {
    if let Some(name) = mark.savepoint {
        host_channel::clear();
        let _ = (ctx.sql_executor)(&format!("RELEASE SAVEPOINT {name}"));
        host_channel::clear();
    }
}

/// A handler takes the error: undo what the block did, then run the handler
/// with `SQLSTATE` and `SQLERRM` set.
pub(super) fn run_handler(
    ctx: &mut ExecutionContext<'_>,
    mark: BlockMark,
    handler: &ExceptionHandler,
    error: &Error,
) -> Result<()> {
    let undone = match &mark.savepoint {
        Some(name) => {
            host_channel::clear();
            let rolled_back = (ctx.sql_executor)(&format!("ROLLBACK TO SAVEPOINT {name}")).is_ok();
            let _ = (ctx.sql_executor)(&format!("RELEASE SAVEPOINT {name}"));
            host_channel::clear();
            rolled_back
        }
        None => false,
    };
    if !undone && ctx.pg.writes > mark.writes {
        return Err(Error::Raised {
            sqlstate: "0A000".to_string(),
            message: format!(
                "an EXCEPTION block cannot undo its statements on this execution path, so its handler was not run \
                 (the error was: {error})"
            ),
            detail: None,
            hint: Some("Call the routine inside a transaction, or with CALL.".to_string()),
        });
    }
    let (detail, hint) = match error {
        Error::Raised { detail, hint, .. } => (detail.clone(), hint.clone()),
        _ => (None, None),
    };
    let caught = Caught {
        sqlstate: sqlstate_of(error),
        message: error.to_string(),
        detail,
        hint,
    };
    ctx.push_scope();
    for (name, value) in [("sqlstate", &caught.sqlstate), ("sqlerrm", &caught.message)] {
        let _ = ctx.scope.declare(
            name.to_string(),
            Variable {
                value: Value::String(value.clone()),
                data_type: Some(crate::DataType::Text),
                is_constant: false,
                not_null: false,
            },
        );
    }
    ctx.pg.handling.push(caught);
    let result = ProceduralExecutor::execute_statements(&handler.body, ctx);
    ctx.pg.handling.pop();
    ctx.pop_scope();
    result
}
