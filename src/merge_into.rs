//! `MERGE INTO` — the SQL-standard statement, in PostgreSQL's syntax.
//!
//! ```sql
//! [WITH …]
//! MERGE INTO target [AS t] USING source [AS s] ON condition
//!   WHEN MATCHED [AND condition] THEN UPDATE SET … | DELETE | DO NOTHING
//!   WHEN NOT MATCHED [BY TARGET] [AND condition] THEN INSERT [(cols)] VALUES (…) | INSERT DEFAULT VALUES | DO NOTHING
//!   WHEN NOT MATCHED BY SOURCE [AND condition] THEN UPDATE SET … | DELETE | DO NOTHING
//!   [RETURNING …]
//! ```
//!
//! How it runs. The statement is one unit of work inside the transaction its
//! caller already has (the implicit one in autocommit): a failure in any
//! action undoes every action of the statement. The source is read once, the
//! target is read once before the first action (so a row this statement
//! inserts is never matched by a later source row), and each action is the
//! ordinary `INSERT`, `UPDATE` or `DELETE` of one row — which is what makes
//! constraints, foreign keys, defaults, serial columns, indexes, branches and
//! row-level security behave exactly as they do for those statements.
//!
//! `MERGE [DATABASE] BRANCH` is a different statement and never comes here.

use crate::sql::evaluator::map_column_refs;
use crate::sql::logical_plan::{BinaryOperator, ReturningItem};
use crate::sql::sql_scan::{self, Tok, TokKind};
use crate::sql::type_inference::TypeInference;
use crate::sql::{Evaluator, LogicalExpr, LogicalPlan};
use crate::{storage, Column, DataType, EmbeddedDatabase, Error, Result, Schema, Tuple, Value};
use sqlparser::ast::{self, Expr};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;

/// The column the `merge_action()` function reads in a RETURNING list.
const ACTION_COLUMN: &str = "\u{1}merge_action";
/// The name of the savepoint a MERGE works under.
const STATEMENT_SAVEPOINT: &str = "\u{1}merge_into";

thread_local! {
    /// The one target row the UPDATE or DELETE a MERGE action is about to run
    /// may touch: `(table, row id)`. That statement takes it when it starts,
    /// so a statement nested in it (a cascade, a trigger) is not gated.
    static ROW_GATE: RefCell<Option<(String, u64)>> = const { RefCell::new(None) };
}

/// Called by UPDATE and DELETE when they start on `table`: the one row id the
/// statement may touch when it is a MERGE action, `None` for any other
/// statement. Taking it clears it.
#[inline]
pub(crate) fn take_row_gate(table: &str) -> Option<u64> {
    ROW_GATE.with(|gate| {
        let mut gate = gate.borrow_mut();
        match gate.as_ref() {
            Some((gated, _)) if gated == table => gate.take().map(|(_, row_id)| row_id),
            _ => None,
        }
    })
}

/// Does a statement that took `gate` from [`take_row_gate`] act on this row?
#[inline]
pub(crate) fn row_gate_admits(gate: Option<u64>, row_id: Option<u64>) -> bool {
    match gate {
        None => true,
        Some(only) => row_id == Some(only),
    }
}

/// Arms [`ROW_GATE`] for one statement; clears it when dropped, taken or not.
struct RowGate;

impl RowGate {
    fn on(table: &str, row_id: u64) -> Self {
        ROW_GATE.with(|gate| *gate.borrow_mut() = Some((table.to_string(), row_id)));
        Self
    }
}

impl Drop for RowGate {
    fn drop(&mut self) {
        ROW_GATE.with(|gate| *gate.borrow_mut() = None);
    }
}

/// Is `sql` a `MERGE INTO` statement (and not `MERGE [DATABASE] BRANCH`)?
pub(crate) fn is_merge_into(sql: &str) -> bool {
    let head = crate::protocol::postgres::handler::pg_skip_leading_comments(sql).trim_start();
    let first = head.as_bytes().get(..5);
    if first.is_some_and(|w| w.eq_ignore_ascii_case(b"MERGE")) {
        let toks = sql_scan::tokens(head);
        return match (toks.first(), toks.get(1)) {
            (Some(merge), Some(next)) if merge.is_word(head, "MERGE") => {
                !next.is_word(head, "BRANCH") && !next.is_word(head, "DATABASE")
            }
            _ => false,
        };
    }
    if head.as_bytes().get(..4).is_some_and(|w| w.eq_ignore_ascii_case(b"WITH"))
        && head
            .as_bytes()
            .windows(5)
            .any(|window| window.eq_ignore_ascii_case(b"MERGE"))
    {
        let toks = sql_scan::tokens(head);
        if !toks.first().is_some_and(|t| t.is_word(head, "WITH")) {
            return false;
        }
        return leading_merge_token(head, &toks).is_some();
    }
    false
}

/// The index of the statement-level `MERGE` keyword: the first token, or the
/// one that ends a leading `WITH …` list.
fn leading_merge_token(sql: &str, toks: &[Tok]) -> Option<usize> {
    let first = toks.first()?;
    if first.is_word(sql, "MERGE") {
        return Some(0);
    }
    if !first.is_word(sql, "WITH") {
        return None;
    }
    for (i, tok) in toks.iter().enumerate().skip(1) {
        if tok.depth != 0 || tok.kind != TokKind::Word {
            continue;
        }
        if tok.is_word(sql, "MERGE") {
            let next = toks.get(i + 1)?;
            if next.is_word(sql, "BRANCH") || next.is_word(sql, "DATABASE") {
                return None;
            }
            return Some(i);
        }
        if tok.is_word(sql, "SELECT")
            || tok.is_word(sql, "INSERT")
            || tok.is_word(sql, "UPDATE")
            || tok.is_word(sql, "DELETE")
            || tok.is_word(sql, "VALUES")
        {
            return None;
        }
    }
    None
}

/// Which rows a `WHEN` clause is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClauseKind {
    Matched,
    NotMatchedByTarget,
    NotMatchedBySource,
}

/// What a `WHEN` clause does, as written.
#[derive(Debug, Clone)]
enum ActionSyntax {
    DoNothing,
    Delete,
    Update(Vec<(ast::Ident, Expr)>),
    Insert { columns: Vec<ast::Ident>, values: Vec<Expr> },
    InsertDefaults,
}

#[derive(Debug, Clone)]
struct ClauseSyntax {
    kind: ClauseKind,
    predicate: Option<Expr>,
    action: ActionSyntax,
}

/// A parsed `MERGE INTO`.
#[derive(Debug, Clone)]
struct MergeSyntax {
    /// `WITH …` in front of the statement, verbatim, or empty.
    with_prefix: String,
    target: ast::ObjectName,
    target_alias: Option<String>,
    /// The source as written after `USING` (a table factor with its alias).
    source_sql: String,
    /// The name the source's columns are qualified with.
    source_name: Option<String>,
    on: Expr,
    clauses: Vec<ClauseSyntax>,
    returning: Option<Vec<ast::SelectItem>>,
}

fn syntax_error(message: impl Into<String>) -> Error {
    Error::sql_state("42601", message)
}

fn unsupported(message: impl Into<String>) -> Error {
    Error::sql_state("0A000", message)
}

/// Split the statement into the parts the SQL parser has grammar for and the
/// parts it has not (`DO NOTHING`, `INSERT DEFAULT VALUES`, `RETURNING`, a
/// leading `WITH`), and parse both.
fn parse(sql: &str) -> Result<MergeSyntax> {
    let sql = sql_scan::statement_text(sql);
    let toks = sql_scan::tokens(sql);
    let merge_at = leading_merge_token(sql, &toks).ok_or_else(|| syntax_error("not a MERGE INTO statement"))?;
    let merge_tok = toks.get(merge_at).ok_or_else(|| syntax_error("empty MERGE statement"))?;
    let with_prefix = sql.get(..merge_tok.start).unwrap_or("").trim().to_string();

    // Statement-level keywords are the ones outside parentheses and outside
    // a CASE … END expression.
    let mut case_depth = 0usize;
    let mut last_when: Option<usize> = None;
    let mut clause_index = 0usize;
    let mut do_nothing: HashSet<usize> = HashSet::new();
    let mut insert_defaults: HashSet<usize> = HashSet::new();
    // (start, end, replacement) over `sql`, in order.
    let mut edits: Vec<(usize, usize, &'static str)> = Vec::new();
    let mut returning_at: Option<usize> = None;
    let mut i = merge_at + 1;
    while let Some(tok) = toks.get(i) {
        i += 1;
        if tok.depth != 0 || tok.kind != TokKind::Word {
            continue;
        }
        if tok.is_word(sql, "CASE") {
            case_depth += 1;
            continue;
        }
        if tok.is_word(sql, "END") {
            case_depth = case_depth.saturating_sub(1);
            continue;
        }
        if case_depth != 0 {
            continue;
        }
        if tok.is_word(sql, "WHEN") {
            last_when = Some(i - 1);
            continue;
        }
        if tok.is_word(sql, "RETURNING") && clause_index > 0 {
            returning_at = Some(i - 1);
            break;
        }
        if !tok.is_word(sql, "THEN") {
            continue;
        }
        let this_clause = clause_index;
        clause_index += 1;
        let (Some(a), b, c) = (toks.get(i), toks.get(i + 1), toks.get(i + 2)) else {
            continue;
        };
        if a.is_word(sql, "DO") && b.is_some_and(|b| b.is_word(sql, "NOTHING")) {
            // The clause's kind decides which action the parser will accept
            // in the placeholder's position.
            let when = last_when.ok_or_else(|| syntax_error("THEN without WHEN in MERGE"))?;
            let not = toks.get(when + 1).is_some_and(|t| t.is_word(sql, "NOT"));
            let by_source = not
                && toks.get(when + 3).is_some_and(|t| t.is_word(sql, "BY"))
                && toks.get(when + 4).is_some_and(|t| t.is_word(sql, "SOURCE"));
            let end = b.map_or(a.end, |b| b.end);
            let placeholder = if not && !by_source { "INSERT VALUES (NULL)" } else { "DELETE" };
            edits.push((a.start, end, placeholder));
            do_nothing.insert(this_clause);
        } else if a.is_word(sql, "INSERT")
            && b.is_some_and(|b| b.is_word(sql, "DEFAULT"))
            && c.is_some_and(|c| c.is_word(sql, "VALUES"))
        {
            let start = b.map_or(a.end, |b| b.start);
            let end = c.map_or(a.end, |c| c.end);
            edits.push((start, end, "VALUES (NULL)"));
            insert_defaults.insert(this_clause);
        }
    }

    let body_end = match returning_at.and_then(|at| toks.get(at)) {
        Some(tok) => tok.start,
        None => sql.len(),
    };
    let returning_sql = returning_at
        .and_then(|at| toks.get(at))
        .and_then(|tok| sql.get(tok.end..))
        .map(str::trim);
    let mut body = String::with_capacity(body_end);
    let mut cursor = merge_tok.start;
    for (start, end, replacement) in edits {
        body.push_str(sql.get(cursor..start).unwrap_or(""));
        body.push_str(replacement);
        cursor = end;
    }
    body.push_str(sql.get(cursor..body_end).unwrap_or(""));

    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let mut statements =
        sqlparser::parser::Parser::parse_sql(&dialect, &body).map_err(|e| syntax_error(format!("MERGE: {e}")))?;
    let statement = match (statements.pop(), statements.is_empty()) {
        (Some(statement), true) => statement,
        _ => return Err(syntax_error("MERGE: expected exactly one statement")),
    };
    let ast::Statement::Merge {
        table,
        source,
        on,
        clauses,
        ..
    } = statement
    else {
        return Err(syntax_error("not a MERGE INTO statement"));
    };

    let (target, target_alias) = match table {
        ast::TableFactor::Table { name, alias, .. } => {
            if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                return Err(unsupported("MERGE: a column alias list on the target is not supported"));
            }
            (name, alias.map(|a| crate::sql::Planner::normalize_ident(&a.name)))
        }
        _ => return Err(Error::sql_state("42809", "MERGE target must be a table")),
    };
    let source_name = match &source {
        ast::TableFactor::Table { name, alias, .. } => match alias {
            Some(alias) => Some(crate::sql::Planner::normalize_ident(&alias.name)),
            None => name.0.last().map(crate::sql::Planner::normalize_ident),
        },
        ast::TableFactor::Derived { alias, .. }
        | ast::TableFactor::TableFunction { alias, .. }
        | ast::TableFactor::Function { alias, .. }
        | ast::TableFactor::UNNEST { alias, .. }
        | ast::TableFactor::NestedJoin { alias, .. } => {
            alias.as_ref().map(|a| crate::sql::Planner::normalize_ident(&a.name))
        }
        _ => None,
    };
    let source_sql = source.to_string();

    if clauses.is_empty() {
        return Err(syntax_error("MERGE needs at least one WHEN clause"));
    }
    let mut out = Vec::with_capacity(clauses.len());
    for (index, clause) in clauses.into_iter().enumerate() {
        let kind = match clause.clause_kind {
            ast::MergeClauseKind::Matched => ClauseKind::Matched,
            ast::MergeClauseKind::NotMatched | ast::MergeClauseKind::NotMatchedByTarget => {
                ClauseKind::NotMatchedByTarget
            }
            ast::MergeClauseKind::NotMatchedBySource => ClauseKind::NotMatchedBySource,
        };
        let action = if do_nothing.contains(&index) {
            ActionSyntax::DoNothing
        } else if insert_defaults.contains(&index) {
            ActionSyntax::InsertDefaults
        } else {
            match clause.action {
                ast::MergeAction::Delete => ActionSyntax::Delete,
                ast::MergeAction::Update { assignments } => {
                    let mut sets = Vec::with_capacity(assignments.len());
                    for assignment in assignments {
                        let column = match assignment.target {
                            ast::AssignmentTarget::ColumnName(name) => match name.0.last() {
                                Some(ident) => ident.clone(),
                                None => return Err(syntax_error("MERGE: empty column name in SET")),
                            },
                            ast::AssignmentTarget::Tuple(_) => {
                                return Err(unsupported(
                                    "MERGE: SET (a, b) = (…) is not supported; assign each column on its own",
                                ))
                            }
                        };
                        sets.push((column, assignment.value));
                    }
                    ActionSyntax::Update(sets)
                }
                ast::MergeAction::Insert(insert) => match insert.kind {
                    ast::MergeInsertKind::Values(values) => {
                        let mut rows = values.rows;
                        let row = match (rows.pop(), rows.is_empty()) {
                            (Some(row), true) => row,
                            _ => return Err(syntax_error("MERGE: INSERT takes exactly one VALUES row")),
                        };
                        ActionSyntax::Insert {
                            columns: insert.columns,
                            values: row,
                        }
                    }
                    ast::MergeInsertKind::Row => return Err(unsupported("MERGE: INSERT ROW is not supported")),
                },
            }
        };
        out.push(ClauseSyntax {
            kind,
            predicate: clause.predicate,
            action,
        });
    }

    let returning = match returning_sql {
        None => None,
        Some("") => return Err(syntax_error("MERGE: RETURNING needs a list")),
        Some(list) => {
            let mut parsed = sqlparser::parser::Parser::parse_sql(&dialect, &format!("SELECT {list}"))
                .map_err(|e| syntax_error(format!("MERGE … RETURNING: {e}")))?;
            match (parsed.pop(), parsed.is_empty()) {
                (Some(ast::Statement::Query(query)), true) => {
                    let ast::Query { body, order_by, .. } = *query;
                    match *body {
                        ast::SetExpr::Select(select)
                            if select.from.is_empty() && select.selection.is_none() && order_by.is_none() =>
                        {
                            Some(select.projection)
                        }
                        _ => return Err(syntax_error("MERGE: RETURNING takes a list of expressions")),
                    }
                }
                _ => return Err(syntax_error("MERGE: RETURNING takes a list of expressions")),
            }
        }
    };

    Ok(MergeSyntax {
        with_prefix,
        target,
        target_alias,
        source_sql,
        source_name,
        on: *on,
        clauses: out,
        returning,
    })
}

/// A `WHEN` clause ready to run.
struct Clause {
    kind: ClauseKind,
    predicate: Option<LogicalExpr>,
    action: Action,
}

enum Action {
    DoNothing,
    Delete,
    /// `(column, value)`; a `None` value is `DEFAULT`.
    Update(Vec<(String, Option<LogicalExpr>)>),
    /// The column list as written (`None`: positional) and one value per
    /// column; a `None` value is `DEFAULT`.
    Insert {
        columns: Option<Vec<String>>,
        values: Vec<Option<LogicalExpr>>,
    },
    InsertDefaults,
}

/// The RETURNING list, bound over `[target row, source row, action]`.
struct Returning {
    exprs: Vec<LogicalExpr>,
    /// The result's columns.
    output: Schema,
    /// The schema the expressions are evaluated over.
    input: Arc<Schema>,
}

/// A MERGE resolved against the catalog.
struct Compiled {
    target: String,
    target_schema: Schema,
    source_plan: LogicalPlan,
    /// Target columns, then source columns.
    combined: Arc<Schema>,
    n_target: usize,
    n_source: usize,
    on: LogicalExpr,
    /// `target column = source expression` conjuncts of the ON condition that
    /// can narrow the target rows examined for one source row.
    equi: Vec<(usize, LogicalExpr)>,
    clauses: Vec<Clause>,
    returning: Option<Returning>,
    /// Positions of the primary-key columns in the target.
    pk: Vec<usize>,
}

/// Which side's columns an expression may name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sides {
    Both,
    /// `WHEN NOT MATCHED [BY TARGET]`: there is no target row.
    SourceOnly,
    /// `WHEN NOT MATCHED BY SOURCE`: there is no source row.
    TargetOnly,
}

/// Does the expression hold a subquery?
fn holds_subquery(expr: &Expr) -> bool {
    struct Finder;
    impl ast::Visitor for Finder {
        type Break = ();
        fn pre_visit_query(&mut self, _query: &ast::Query) -> ControlFlow<()> {
            ControlFlow::Break(())
        }
    }
    use ast::Visit;
    expr.visit(&mut Finder).is_break()
}

struct Binder<'a, 'p> {
    planner: &'a crate::sql::Planner<'p>,
    schema: &'a Schema,
    n_target: usize,
    n_source: usize,
    target_label: &'a str,
    source_label: &'a str,
}

impl Binder<'_, '_> {
    /// Turn a written expression into one the evaluator runs over the
    /// combined row, refusing what cannot be answered correctly.
    fn bind(&self, expr: &Expr, sides: Sides, what: &str) -> Result<LogicalExpr> {
        if holds_subquery(expr) {
            return Err(unsupported(format!(
                "MERGE: a subquery in {what} is not supported; compute it in the USING source instead"
            )));
        }
        let logical = self.planner.expr_to_logical(expr)?;
        let mut problem: Option<Error> = None;
        let checked = map_column_refs(logical, &mut |table, name| {
            if problem.is_none() {
                if let Err(e) = self.check_column(table.as_deref(), &name, sides, what) {
                    problem = Some(e);
                }
            }
            LogicalExpr::Column { table, name }
        });
        match problem {
            Some(e) => Err(e),
            None => Ok(checked),
        }
    }

    fn check_column(&self, table: Option<&str>, name: &str, sides: Sides, what: &str) -> Result<()> {
        let in_target = |index: usize| index < self.n_target;
        let in_source = |index: usize| index >= self.n_target && index < self.n_target + self.n_source;
        let index = match table {
            Some(qualifier) => self
                .schema
                .get_qualified_column_index(Some(qualifier), name)
                .ok_or_else(|| {
                    Error::sql_state(
                        "42703",
                        format!("column {qualifier}.{name} does not exist in {what} of MERGE"),
                    )
                })?,
            None => {
                let mut hits = self
                    .schema
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| column.name == name)
                    .map(|(index, _)| index);
                let first = hits.next().ok_or_else(|| {
                    Error::sql_state("42703", format!("column \"{name}\" does not exist in {what} of MERGE"))
                })?;
                if hits.any(|other| in_target(other) != in_target(first)) {
                    return Err(Error::sql_state(
                        "42702",
                        format!("column reference \"{name}\" is ambiguous"),
                    ));
                }
                first
            }
        };
        if in_target(index) && sides == Sides::SourceOnly {
            return Err(Error::sql_state(
                "42P01",
                format!(
                    "invalid reference to FROM-clause entry for table \"{}\": a WHEN NOT MATCHED clause has no \
                     target row, so {what} cannot use column \"{name}\" of the target",
                    self.target_label
                ),
            ));
        }
        if in_source(index) && sides == Sides::TargetOnly {
            return Err(Error::sql_state(
                "42P01",
                format!(
                    "invalid reference to FROM-clause entry for table \"{}\": a WHEN NOT MATCHED BY SOURCE clause \
                     has no source row, so {what} cannot use column \"{name}\" of the source",
                    self.source_label
                ),
            ));
        }
        Ok(())
    }

    /// Which sides an already bound expression reads.
    fn sides_used(&self, expr: &LogicalExpr) -> (bool, bool) {
        let mut target = false;
        let mut source = false;
        let _ = map_column_refs(expr.clone(), &mut |table, name| {
            if let Some(index) = self.schema.get_qualified_column_index(table.as_deref(), &name) {
                if index < self.n_target {
                    target = true;
                } else {
                    source = true;
                }
            }
            LogicalExpr::Column { table, name }
        });
        (target, source)
    }
}

/// `DEFAULT` written as a value.
fn is_default_keyword(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(ident) if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("DEFAULT"))
}

/// The conjuncts of `a AND b AND …`.
fn conjuncts<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    match expr {
        Expr::BinaryOp {
            left,
            op: ast::BinaryOperator::And,
            right,
        } => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        Expr::Nested(inner) => conjuncts(inner, out),
        other => out.push(other),
    }
}

/// The equality key families a hash lookup answers exactly as `=` does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyFamily {
    Integer,
    Text,
    Uuid,
    Boolean,
}

fn column_key_family(data_type: &DataType) -> Option<KeyFamily> {
    match data_type {
        DataType::Int2 | DataType::Int4 | DataType::Int8 => Some(KeyFamily::Integer),
        DataType::Text | DataType::Varchar(_) => Some(KeyFamily::Text),
        DataType::Uuid => Some(KeyFamily::Uuid),
        DataType::Boolean => Some(KeyFamily::Boolean),
        _ => None,
    }
}

/// A value as a hash key of its family; `None` when it is not of the family
/// (the caller then compares row by row instead).
fn key_of(value: &Value, family: KeyFamily) -> Option<Value> {
    match (family, value) {
        (KeyFamily::Integer, Value::Int2(v)) => Some(Value::Int8(i64::from(*v))),
        (KeyFamily::Integer, Value::Int4(v)) => Some(Value::Int8(i64::from(*v))),
        (KeyFamily::Integer, Value::Int8(v)) => Some(Value::Int8(*v)),
        (KeyFamily::Text, Value::String(s)) => Some(Value::String(s.clone())),
        (KeyFamily::Uuid, Value::Uuid(_)) | (KeyFamily::Boolean, Value::Boolean(_)) => Some(value.clone()),
        _ => None,
    }
}

fn truthy(value: &Value) -> bool {
    matches!(value, Value::Boolean(true))
}

/// What one action did, for the count and for RETURNING.
struct Applied {
    /// The row as the action left it (the removed row for a DELETE).
    row: Option<Tuple>,
    label: &'static str,
}

impl EmbeddedDatabase {
    /// Resolve a parsed MERGE against the catalog as this session sees it.
    fn compile_merge(&self, syntax: &MergeSyntax) -> Result<Compiled> {
        let catalog = self.storage.catalog();
        let planner = crate::sql::Planner::with_catalog(&catalog)
            .with_current_schema(self.current_schema())
            .with_search_path(self.current_search_path());
        let target = planner.resolve_table_ref(&syntax.target);
        let display_name = syntax.target.to_string();

        // The target is a base table. A view or a materialized view has a
        // schema too, so without this a MERGE into one would run against
        // nothing and report success.
        if self.storage.view_catalog().view_exists(&target)? {
            return Err(Error::Raised {
                sqlstate: "42809".to_string(),
                message: format!("cannot execute MERGE on relation \"{display_name}\""),
                detail: Some("This operation is not supported for views.".to_string()),
                hint: None,
            });
        }
        if self.storage.mv_catalog().view_exists(&target)? {
            return Err(Error::Raised {
                sqlstate: "42809".to_string(),
                message: format!("cannot execute MERGE on relation \"{display_name}\""),
                detail: Some("This operation is not supported for materialized views.".to_string()),
                hint: None,
            });
        }
        let target_schema = catalog.get_table_schema(&target)?;
        let bare_target = target.rsplit('.').next().unwrap_or(&target).to_string();
        let target_label = syntax.target_alias.clone().unwrap_or(bare_target);

        // The source: whatever follows USING, read as a query of its own.
        let source_sql = if syntax.with_prefix.is_empty() {
            format!("SELECT * FROM {}", syntax.source_sql)
        } else {
            format!("{} SELECT * FROM {}", syntax.with_prefix, syntax.source_sql)
        };
        let (statement, _) = self.parse_cached(&source_sql)?;
        let source_plan = crate::sql::Planner::with_catalog(&catalog)
            .with_sql(source_sql.clone())
            .with_current_schema(self.current_schema())
            .with_search_path(self.current_search_path())
            .statement_to_plan(statement)?;
        let source_schema = source_plan.schema();
        let source_label = syntax.source_name.clone().unwrap_or_default();
        if !source_label.is_empty() && source_label == target_label {
            return Err(Error::sql_state(
                "42712",
                format!("table name \"{source_label}\" specified more than once"),
            ));
        }

        let n_target = target_schema.columns.len();
        let n_source = source_schema.columns.len();
        let mut columns: Vec<Column> = Vec::with_capacity(n_target + n_source);
        for column in &target_schema.columns {
            let mut column = column.clone();
            column.source_table = Some(target_label.clone());
            column.source_table_name = Some(target_label.clone());
            columns.push(column);
        }
        for column in &source_schema.columns {
            let mut column = column.clone();
            let label = (!source_label.is_empty()).then(|| source_label.clone());
            column.source_table = label.clone();
            column.source_table_name = label;
            column.primary_key = false;
            columns.push(column);
        }
        let combined = Schema { columns };
        let binder = Binder {
            planner: &planner,
            schema: &combined,
            n_target,
            n_source,
            target_label: &target_label,
            source_label: &source_label,
        };

        let on = binder.bind(&syntax.on, Sides::Both, "the ON condition")?;
        let mut equi = Vec::new();
        let mut parts = Vec::new();
        conjuncts(&syntax.on, &mut parts);
        for part in parts {
            let Expr::BinaryOp {
                left,
                op: ast::BinaryOperator::Eq,
                right,
            } = part
            else {
                continue;
            };
            let (Ok(l), Ok(r)) = (
                binder.bind(left, Sides::Both, "the ON condition"),
                binder.bind(right, Sides::Both, "the ON condition"),
            ) else {
                continue;
            };
            for (column_side, value_side) in [(&l, &r), (&r, &l)] {
                let LogicalExpr::Column { table, name } = column_side else {
                    continue;
                };
                let Some(index) = combined.get_qualified_column_index(table.as_deref(), name) else {
                    continue;
                };
                if index >= n_target || binder.sides_used(value_side).0 {
                    continue;
                }
                let family = combined.columns.get(index).and_then(|c| column_key_family(&c.data_type));
                if family.is_some() {
                    equi.push((index, value_side.clone()));
                    break;
                }
            }
        }

        let mut clauses = Vec::with_capacity(syntax.clauses.len());
        for clause in &syntax.clauses {
            let sides = match clause.kind {
                ClauseKind::Matched => Sides::Both,
                ClauseKind::NotMatchedByTarget => Sides::SourceOnly,
                ClauseKind::NotMatchedBySource => Sides::TargetOnly,
            };
            let predicate = match &clause.predicate {
                Some(expr) => Some(binder.bind(expr, sides, "a WHEN condition")?),
                None => None,
            };
            let action = match &clause.action {
                ActionSyntax::DoNothing => Action::DoNothing,
                ActionSyntax::Delete => Action::Delete,
                ActionSyntax::InsertDefaults => Action::InsertDefaults,
                ActionSyntax::Update(sets) => {
                    let mut seen = HashSet::new();
                    let mut out = Vec::with_capacity(sets.len());
                    for (ident, value) in sets {
                        let name = crate::sql::Planner::normalize_ident(ident);
                        let column = target_schema.get_column(&name).ok_or_else(|| {
                            Error::sql_state(
                                "42703",
                                format!("column \"{name}\" of relation \"{display_name}\" does not exist"),
                            )
                        })?;
                        if !seen.insert(name.clone()) {
                            return Err(syntax_error(format!("multiple assignments to same column \"{name}\"")));
                        }
                        let value = if is_default_keyword(value) {
                            None
                        } else {
                            Some(binder.bind(value, sides, "a SET expression")?)
                        };
                        out.push((column.name.clone(), value));
                    }
                    Action::Update(out)
                }
                ActionSyntax::Insert { columns, values } => {
                    let names: Option<Vec<String>> = if columns.is_empty() {
                        None
                    } else {
                        let mut names = Vec::with_capacity(columns.len());
                        for ident in columns {
                            let name = crate::sql::Planner::normalize_ident(ident);
                            if target_schema.get_column(&name).is_none() {
                                return Err(Error::sql_state(
                                    "42703",
                                    format!("column \"{name}\" of relation \"{display_name}\" does not exist"),
                                ));
                            }
                            names.push(name);
                        }
                        Some(names)
                    };
                    let expected = names.as_ref().map_or(n_target, Vec::len);
                    if values.len() > expected || (names.is_some() && values.len() != expected) {
                        return Err(syntax_error(format!(
                            "MERGE: INSERT has {} value(s) for {} column(s)",
                            values.len(),
                            expected
                        )));
                    }
                    let mut out = Vec::with_capacity(values.len());
                    for value in values {
                        out.push(if is_default_keyword(value) {
                            None
                        } else {
                            Some(binder.bind(value, sides, "an INSERT value")?)
                        });
                    }
                    Action::Insert {
                        columns: names,
                        values: out,
                    }
                }
            };
            clauses.push(Clause {
                kind: clause.kind,
                predicate,
                action,
            });
        }

        let returning = match &syntax.returning {
            None => None,
            Some(items) => Some(self.bind_merge_returning(items, &planner, &combined, n_target, n_source, &target_label, &source_label)?),
        };

        let pk = target_schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.primary_key)
            .map(|(index, _)| index)
            .collect();

        Ok(Compiled {
            target,
            target_schema,
            source_plan,
            combined: Arc::new(combined),
            n_target,
            n_source,
            on,
            equi,
            clauses,
            returning,
            pk,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_merge_returning(
        &self,
        items: &[ast::SelectItem],
        planner: &crate::sql::Planner<'_>,
        combined: &Schema,
        n_target: usize,
        n_source: usize,
        target_label: &str,
        source_label: &str,
    ) -> Result<Returning> {
        let mut input = combined.clone();
        input.columns.push(Column::new(ACTION_COLUMN, DataType::Text));
        let binder = Binder {
            planner,
            schema: &input,
            n_target,
            n_source,
            target_label,
            source_label,
        };
        let column_ref = |index: usize| -> Option<(LogicalExpr, Column)> {
            let column = input.columns.get(index)?;
            let mut out = column.clone();
            out.primary_key = false;
            out.default_expr = None;
            Some((
                LogicalExpr::Column {
                    table: column.source_table.clone(),
                    name: column.name.clone(),
                },
                out,
            ))
        };
        let mut exprs = Vec::new();
        let mut output = Vec::new();
        for item in items {
            match item {
                // `*`: the source's columns, then the target's.
                ast::SelectItem::Wildcard(_) => {
                    for index in (n_target..n_target + n_source).chain(0..n_target) {
                        if let Some((expr, column)) = column_ref(index) {
                            exprs.push(expr);
                            output.push(column);
                        }
                    }
                }
                ast::SelectItem::QualifiedWildcard(name, _) => {
                    let label = name.0.last().map(crate::sql::Planner::normalize_ident).unwrap_or_default();
                    let range = if label == target_label {
                        0..n_target
                    } else if !source_label.is_empty() && label == source_label {
                        n_target..n_target + n_source
                    } else {
                        return Err(Error::sql_state(
                            "42P01",
                            format!("missing FROM-clause entry for table \"{label}\""),
                        ));
                    };
                    for index in range {
                        if let Some((expr, column)) = column_ref(index) {
                            exprs.push(expr);
                            output.push(column);
                        }
                    }
                }
                ast::SelectItem::UnnamedExpr(expr) | ast::SelectItem::ExprWithAlias { expr, .. } => {
                    let mut expr = expr.clone();
                    let mut calls_action = false;
                    let _ = ast::visit_expressions_mut(&mut expr, |e: &mut Expr| {
                        if let Expr::Function(function) = e {
                            let name = function.name.0.last().map(|i| i.value.to_ascii_lowercase());
                            let no_args = matches!(&function.args, ast::FunctionArguments::List(list) if list.args.is_empty());
                            if name.as_deref() == Some("merge_action") && no_args {
                                *e = Expr::Identifier(ast::Ident::with_quote('"', ACTION_COLUMN));
                                calls_action = true;
                            }
                        }
                        ControlFlow::<()>::Continue(())
                    });
                    let name = match item {
                        ast::SelectItem::ExprWithAlias { alias, .. } => crate::sql::Planner::normalize_ident(alias),
                        _ => match &expr {
                            Expr::Identifier(ident) if calls_action && ident.value == ACTION_COLUMN => {
                                "merge_action".to_string()
                            }
                            Expr::Identifier(ident) => crate::sql::Planner::normalize_ident(ident),
                            Expr::CompoundIdentifier(parts) => parts
                                .last()
                                .map(crate::sql::Planner::normalize_ident)
                                .unwrap_or_else(|| "?column?".to_string()),
                            Expr::Function(function) => function
                                .name
                                .0
                                .last()
                                .map(|i| i.value.to_lowercase())
                                .unwrap_or_else(|| "?column?".to_string()),
                            _ => "?column?".to_string(),
                        },
                    };
                    let bound = binder.bind(&expr, Sides::Both, "the RETURNING list")?;
                    let mut column = bound.to_column(name, &input);
                    column.primary_key = false;
                    column.source_table = None;
                    column.source_table_name = None;
                    exprs.push(bound);
                    output.push(column);
                }
            }
        }
        Ok(Returning {
            exprs,
            output: Schema { columns: output },
            input: Arc::new(input),
        })
    }

    /// The columns a `MERGE … RETURNING` answers with; `None` for a MERGE
    /// without the clause.
    pub(crate) fn merge_returning_schema(&self, sql: &str) -> Result<Option<Schema>> {
        let syntax = parse(sql)?;
        if syntax.returning.is_none() {
            return Ok(None);
        }
        Ok(self.compile_merge(&syntax)?.returning.map(|r| r.output))
    }

    /// [`Self::merge_returning_schema`] for a statement that may be something
    /// else: `None` unless it is a `MERGE INTO` with a RETURNING list.
    pub(crate) fn merge_returning_schema_if_merge(&self, sql: &str) -> Result<Option<Schema>> {
        if !is_merge_into(sql) {
            return Ok(None);
        }
        self.merge_returning_schema(sql)
    }

    /// Run one `MERGE INTO` inside `txn`: the number of rows it inserted,
    /// updated or deleted, and its RETURNING rows.
    pub(crate) fn execute_merge_into(
        &self,
        sql: &str,
        params: &[Value],
        txn: &storage::Transaction,
    ) -> Result<(u64, Vec<Tuple>)> {
        let syntax = parse(sql)?;
        let compiled = self.compile_merge(&syntax)?;
        // One statement, one unit: whatever an action changed is undone when
        // a later one fails, also inside a transaction block that goes on.
        self.savepoint_establish(txn, STATEMENT_SAVEPOINT);
        let outcome = self.run_merge(&compiled, params, txn);
        if outcome.is_err() {
            // The statement's own error is the one to report.
            let _ = self.savepoint_rollback_to(txn, STATEMENT_SAVEPOINT);
        }
        let _ = Self::savepoint_release(txn, STATEMENT_SAVEPOINT);
        outcome
    }

    fn run_merge(&self, m: &Compiled, params: &[Value], txn: &storage::Transaction) -> Result<(u64, Vec<Tuple>)> {
        let source_rows = self.query_plan_with_params(&m.source_plan, params, Some(txn))?;

        // The target as this transaction sees it, read once.
        let branch_view = self.storage.branch_view()?;
        let target_rows: Vec<Tuple> = match &branch_view {
            Some(view) => view.scan(Some(txn), &m.target, &m.target_schema)?,
            None => {
                let base = self.storage.scan_table_branch_aware(&m.target)?;
                txn.merge_with_write_set_shaped(&m.target, base, &self.storage.row_shape(&m.target, m.n_target)?)?
            }
        };

        let evaluator = Evaluator::with_parameters(Arc::clone(&m.combined), params.to_vec());
        let null_target = vec![Value::Null; m.n_target];
        let null_source = vec![Value::Null; m.n_source];
        let combine = |target: &[Value], source: &[Value]| -> Tuple {
            let mut values = Vec::with_capacity(m.n_target + m.n_source);
            values.extend_from_slice(target);
            values.extend_from_slice(source);
            Tuple::new(values)
        };
        // A source row narrower or wider than its schema cannot be addressed
        // by column position.
        if let Some(bad) = source_rows.iter().find(|row| row.values.len() != m.n_source) {
            return Err(Error::internal(format!(
                "MERGE: a source row has {} values for {} columns",
                bad.values.len(),
                m.n_source
            )));
        }

        // Equality keys of the ON condition narrow the target rows one
        // source row is compared with; the full condition is still evaluated
        // on each candidate.
        let key_families: Vec<Option<KeyFamily>> = m
            .equi
            .iter()
            .map(|(index, _)| m.combined.columns.get(*index).and_then(|c| column_key_family(&c.data_type)))
            .collect();
        let index: Option<HashMap<Vec<Value>, Vec<usize>>> = if m.equi.is_empty() {
            None
        } else {
            let mut map: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
            'rows: for (position, row) in target_rows.iter().enumerate() {
                let mut key = Vec::with_capacity(m.equi.len());
                for ((column, _), family) in m.equi.iter().zip(&key_families) {
                    let (Some(value), Some(family)) = (row.values.get(*column), family) else {
                        continue 'rows;
                    };
                    match key_of(value, *family) {
                        Some(k) => key.push(k),
                        // NULL (never equal) or a value outside its column's
                        // family: not reachable through the index.
                        None => continue 'rows,
                    }
                }
                map.entry(key).or_default().push(position);
            }
            Some(map)
        };
        // Target rows the index cannot hold are compared row by row when a
        // lookup is used; with no such rows this is empty.
        let unindexed: Vec<usize> = match &index {
            None => Vec::new(),
            Some(map) => {
                let indexed: HashSet<usize> = map.values().flatten().copied().collect();
                (0..target_rows.len()).filter(|i| !indexed.contains(i)).collect()
            }
        };
        let all_positions: Vec<usize> = (0..target_rows.len()).collect();

        let mut matched_by_source = vec![false; target_rows.len()];
        let mut changed: HashSet<usize> = HashSet::new();
        let mut count = 0u64;
        let mut returned: Vec<Tuple> = Vec::new();
        let any_by_source = m.clauses.iter().any(|c| c.kind == ClauseKind::NotMatchedBySource);

        for source in &source_rows {
            let source_only = combine(&null_target, &source.values);
            // Candidates for this source row.
            let mut lookup: Option<Vec<usize>> = None;
            if let Some(map) = &index {
                let mut key = Vec::with_capacity(m.equi.len());
                let mut usable = true;
                let mut never = false;
                for ((_, expr), family) in m.equi.iter().zip(&key_families) {
                    let value = evaluator.evaluate(expr, &source_only)?;
                    if matches!(value, Value::Null) {
                        never = true;
                        break;
                    }
                    match family.and_then(|family| key_of(&value, family)) {
                        Some(k) => key.push(k),
                        None => {
                            usable = false;
                            break;
                        }
                    }
                }
                if never {
                    lookup = Some(Vec::new());
                } else if usable {
                    let mut candidates = map.get(&key).cloned().unwrap_or_default();
                    candidates.extend_from_slice(&unindexed);
                    candidates.sort_unstable();
                    lookup = Some(candidates);
                }
            }
            let candidates: &[usize] = lookup.as_deref().unwrap_or(&all_positions);

            let mut any_match = false;
            for &position in candidates {
                let Some(target) = target_rows.get(position) else {
                    continue;
                };
                let both = combine(&target.values, &source.values);
                if !truthy(&evaluator.evaluate(&m.on, &both)?) {
                    continue;
                }
                any_match = true;
                if let Some(flag) = matched_by_source.get_mut(position) {
                    *flag = true;
                }
                let Some(clause) = Self::first_clause(m, ClauseKind::Matched, &evaluator, &both)? else {
                    continue;
                };
                if matches!(clause.action, Action::DoNothing) {
                    continue;
                }
                if !changed.insert(position) {
                    return Err(Error::Raised {
                        sqlstate: "21000".to_string(),
                        message: "MERGE command cannot affect row a second time".to_string(),
                        detail: None,
                        hint: Some(
                            "Ensure that not more than one source row matches any one target row.".to_string(),
                        ),
                    });
                }
                if let Some(applied) =
                    self.apply_merge_action(m, clause, target, &both, &evaluator, txn, m.returning.is_some())?
                {
                    count += 1;
                    self.merge_returning_row(m, &applied, &source.values, params, &mut returned)?;
                }
            }
            if any_match {
                continue;
            }
            let Some(clause) = Self::first_clause(m, ClauseKind::NotMatchedByTarget, &evaluator, &source_only)? else {
                continue;
            };
            if let Some(applied) = self.apply_merge_insert(m, clause, &source_only, &evaluator, txn)? {
                count += 1;
                self.merge_returning_row(m, &applied, &source.values, params, &mut returned)?;
            }
        }

        if any_by_source {
            for (position, target) in target_rows.iter().enumerate() {
                if matched_by_source.get(position).copied().unwrap_or(false) {
                    continue;
                }
                let target_only = combine(&target.values, &null_source);
                let Some(clause) = Self::first_clause(m, ClauseKind::NotMatchedBySource, &evaluator, &target_only)?
                else {
                    continue;
                };
                if matches!(clause.action, Action::DoNothing) {
                    continue;
                }
                if let Some(applied) =
                    self.apply_merge_action(m, clause, target, &target_only, &evaluator, txn, m.returning.is_some())?
                {
                    count += 1;
                    self.merge_returning_row(m, &applied, &null_source, params, &mut returned)?;
                }
            }
        }
        Ok((count, returned))
    }

    /// The first clause of `kind` whose condition holds for `row`.
    fn first_clause<'m>(
        m: &'m Compiled,
        kind: ClauseKind,
        evaluator: &Evaluator,
        row: &Tuple,
    ) -> Result<Option<&'m Clause>> {
        for clause in m.clauses.iter().filter(|c| c.kind == kind) {
            let holds = match &clause.predicate {
                Some(predicate) => truthy(&evaluator.evaluate(predicate, row)?),
                None => true,
            };
            if holds {
                return Ok(Some(clause));
            }
        }
        Ok(None)
    }

    /// The value `SET column = DEFAULT` assigns.
    fn merge_column_default(&self, m: &Compiled, column: &str) -> Result<LogicalExpr> {
        let Some(definition) = m.target_schema.get_column(column) else {
            return Ok(LogicalExpr::Literal(Value::Null));
        };
        match &definition.default_expr {
            Some(json) => serde_json::from_str::<LogicalExpr>(json)
                .map_err(|e| Error::query_execution(format!("cannot apply the DEFAULT of column \"{column}\": {e}"))),
            None if self
                .storage
                .catalog()
                .is_identity_column(&m.target, column)
                .unwrap_or(false) =>
            {
                Err(unsupported(format!(
                    "SET {column} = DEFAULT on a serial column is not supported"
                )))
            }
            None => Ok(LogicalExpr::Literal(Value::Null)),
        }
    }

    /// `WHERE` for the one target row: its primary key when it has one (a
    /// point lookup); the row gate alone makes it exact either way.
    fn merge_row_selection(m: &Compiled, target: &Tuple) -> Option<LogicalExpr> {
        let mut selection: Option<LogicalExpr> = None;
        for &index in &m.pk {
            let (Some(column), Some(value)) = (m.target_schema.columns.get(index), target.values.get(index)) else {
                return None;
            };
            let equals = LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column {
                    table: None,
                    name: column.name.clone(),
                }),
                op: BinaryOperator::Eq,
                right: Box::new(LogicalExpr::Literal(value.clone())),
            };
            selection = Some(match selection {
                None => equals,
                Some(previous) => LogicalExpr::BinaryExpr {
                    left: Box::new(previous),
                    op: BinaryOperator::And,
                    right: Box::new(equals),
                },
            });
        }
        selection
    }

    /// Run the UPDATE or DELETE of one matched target row. `None`: the row
    /// was not changed (no longer there, or not this caller's to change).
    #[allow(clippy::too_many_arguments)]
    fn apply_merge_action(
        &self,
        m: &Compiled,
        clause: &Clause,
        target: &Tuple,
        row: &Tuple,
        evaluator: &Evaluator,
        txn: &storage::Transaction,
        want_row: bool,
    ) -> Result<Option<Applied>> {
        let Some(row_id) = target.row_id else {
            return Err(Error::internal("MERGE: a target row has no row id"));
        };
        let returning = want_row.then(|| vec![ReturningItem::Wildcard]);
        let (plan, label) = match &clause.action {
            Action::Update(sets) => {
                let mut assignments = Vec::with_capacity(sets.len());
                for (column, value) in sets {
                    let value = match value {
                        Some(expr) => LogicalExpr::Literal(evaluator.evaluate(expr, row)?),
                        None => self.merge_column_default(m, column)?,
                    };
                    assignments.push((column.clone(), value));
                }
                (
                    LogicalPlan::Update {
                        table_name: m.target.clone(),
                        assignments,
                        selection: Self::merge_row_selection(m, target),
                        returning,
                    },
                    "UPDATE",
                )
            }
            Action::Delete => (
                LogicalPlan::Delete {
                    table_name: m.target.clone(),
                    selection: Self::merge_row_selection(m, target),
                    returning,
                },
                "DELETE",
            ),
            Action::DoNothing => return Ok(None),
            Action::Insert { .. } | Action::InsertDefaults => {
                return Err(Error::internal("MERGE: INSERT is not an action for a matched row"))
            }
        };
        let (affected, mut rows) = {
            let _gate = RowGate::on(&m.target, row_id);
            self.execute_plan_with_params(&plan, &[], Some(txn))?
        };
        if affected == 0 {
            return Ok(None);
        }
        Ok(Some(Applied {
            row: rows.pop(),
            label,
        }))
    }

    /// Run the INSERT of a `WHEN NOT MATCHED` clause for one source row.
    fn apply_merge_insert(
        &self,
        m: &Compiled,
        clause: &Clause,
        row: &Tuple,
        evaluator: &Evaluator,
        txn: &storage::Transaction,
    ) -> Result<Option<Applied>> {
        let (columns, values) = match &clause.action {
            Action::DoNothing => return Ok(None),
            Action::InsertDefaults => (None, vec![LogicalExpr::DefaultValue; m.n_target]),
            Action::Insert { columns, values } => {
                let mut out = Vec::with_capacity(values.len());
                for value in values {
                    out.push(match value {
                        Some(expr) => LogicalExpr::Literal(evaluator.evaluate(expr, row)?),
                        None => LogicalExpr::DefaultValue,
                    });
                }
                // A positional list shorter than the table leaves the rest
                // to their defaults.
                if columns.is_none() {
                    while out.len() < m.n_target {
                        out.push(LogicalExpr::DefaultValue);
                    }
                }
                (columns.clone(), out)
            }
            Action::Update(_) | Action::Delete => {
                return Err(Error::internal("MERGE: only INSERT is an action for an unmatched source row"))
            }
        };
        let plan = LogicalPlan::Insert {
            table_name: m.target.clone(),
            columns,
            values: vec![values],
            returning: m.returning.is_some().then(|| vec![ReturningItem::Wildcard]),
            on_conflict: None,
        };
        let (affected, mut rows) = self.execute_plan_with_params(&plan, &[], Some(txn))?;
        if affected == 0 {
            return Ok(None);
        }
        Ok(Some(Applied {
            row: rows.pop(),
            label: "INSERT",
        }))
    }

    /// Append the RETURNING row of one action.
    fn merge_returning_row(
        &self,
        m: &Compiled,
        applied: &Applied,
        source: &[Value],
        params: &[Value],
        out: &mut Vec<Tuple>,
    ) -> Result<()> {
        let Some(returning) = &m.returning else {
            return Ok(());
        };
        let Some(row) = &applied.row else {
            return Err(Error::internal("MERGE: an action reported no row for RETURNING"));
        };
        if row.values.len() != m.n_target {
            return Err(Error::internal("MERGE: an action reported a row of the wrong width"));
        }
        let mut values = Vec::with_capacity(m.n_target + m.n_source + 1);
        values.extend_from_slice(&row.values);
        values.extend_from_slice(source);
        values.push(Value::String(applied.label.to_string()));
        let input = Tuple::new(values);
        let evaluator = Evaluator::with_parameters(Arc::clone(&returning.input), params.to_vec());
        let mut result = Vec::with_capacity(returning.exprs.len());
        for expr in &returning.exprs {
            result.push(evaluator.evaluate(expr, &input)?);
        }
        out.push(Tuple::new(result));
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn merge_into_is_told_from_merge_branch() {
        assert!(is_merge_into("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE"));
        assert!(is_merge_into("  -- c\n merge into t using s on true when matched then delete"));
        assert!(is_merge_into("WITH s AS (SELECT 1 AS id) MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE"));
        assert!(!is_merge_into("MERGE BRANCH dev INTO main"));
        assert!(!is_merge_into("MERGE DATABASE BRANCH dev INTO main"));
        assert!(!is_merge_into("MERGE BRANCH dev ABORT"));
        assert!(!is_merge_into("WITH s AS (SELECT 'merge') SELECT * FROM s"));
        assert!(!is_merge_into("SELECT 1"));
    }

    #[test]
    fn the_forms_the_parser_lacks_are_read() {
        let m = parse(
            "MERGE INTO t AS x USING (SELECT 1 AS id, 'THEN DO NOTHING' AS v) AS s ON x.id = s.id \
             WHEN MATCHED AND s.v = 'a' THEN DO NOTHING \
             WHEN MATCHED THEN UPDATE SET v = CASE WHEN s.id = 1 THEN 'one' ELSE s.v END \
             WHEN NOT MATCHED AND s.id > 5 THEN DO NOTHING \
             WHEN NOT MATCHED BY TARGET THEN INSERT DEFAULT VALUES \
             WHEN NOT MATCHED BY SOURCE THEN DO NOTHING \
             RETURNING merge_action(), x.*;",
        )
        .unwrap();
        assert_eq!(m.target_alias.as_deref(), Some("x"));
        assert_eq!(m.source_name.as_deref(), Some("s"));
        let kinds: Vec<ClauseKind> = m.clauses.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ClauseKind::Matched,
                ClauseKind::Matched,
                ClauseKind::NotMatchedByTarget,
                ClauseKind::NotMatchedByTarget,
                ClauseKind::NotMatchedBySource
            ]
        );
        assert!(matches!(m.clauses[0].action, ActionSyntax::DoNothing));
        assert!(matches!(m.clauses[1].action, ActionSyntax::Update(_)));
        assert!(matches!(m.clauses[2].action, ActionSyntax::DoNothing));
        assert!(matches!(m.clauses[3].action, ActionSyntax::InsertDefaults));
        assert!(matches!(m.clauses[4].action, ActionSyntax::DoNothing));
        assert_eq!(m.returning.unwrap().len(), 2);
    }

    #[test]
    fn a_leading_with_is_kept_for_the_source() {
        let m = parse("WITH s AS (SELECT 1 AS id) MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE").unwrap();
        assert_eq!(m.with_prefix, "WITH s AS (SELECT 1 AS id)");
        assert_eq!(m.source_sql, "s");
    }

    #[test]
    fn the_row_gate_is_taken_once_by_its_table() {
        assert_eq!(take_row_gate("t"), None);
        {
            let _gate = RowGate::on("t", 3);
            assert_eq!(take_row_gate("other"), None);
            let gate = take_row_gate("t");
            assert_eq!(gate, Some(3));
            assert!(row_gate_admits(gate, Some(3)));
            assert!(!row_gate_admits(gate, Some(7)));
            assert!(!row_gate_admits(gate, None));
            // A statement nested in the gated one is not gated.
            assert_eq!(take_row_gate("t"), None);
        }
        {
            let _gate = RowGate::on("t", 3);
        }
        assert_eq!(take_row_gate("t"), None);
        assert!(row_gate_admits(None, Some(1)));
    }
}
