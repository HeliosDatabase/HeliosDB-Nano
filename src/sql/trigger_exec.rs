//! Running a trigger's function.
//!
//! A trigger names a function (`EXECUTE FUNCTION f(args)`) or carries an
//! inline body (the MySQL and SQLite forms, see [`super::trigger_syntax`]).
//! When it fires, the body runs through the PL/pgSQL interpreter
//! ([`super::procedural`]) with the trigger function's world laid over it:
//!
//! * `NEW.<column>` and `OLD.<column>` read the row being written,
//! * `NEW.<column> := <expression>` changes it (the caller decides whether the
//!   change is kept: only a BEFORE row trigger's is),
//! * `TG_OP`, `TG_NAME`, `TG_WHEN`, `TG_LEVEL`, `TG_TABLE_NAME`,
//!   `TG_TABLE_SCHEMA`, `TG_RELNAME`, `TG_NARGS` and `TG_ARGV[n]`,
//! * `RETURN NEW`, `RETURN OLD` and `RETURN NULL`,
//! * `RAISE` with a `%` format string: `EXCEPTION` fails the statement with
//!   the given SQLSTATE (P0001 when none is given), the other levels are
//!   handed to the caller as notices.
//!
//! The interpreter itself is used through its ordinary entry points
//! (`ProceduralParser::parse_block`, `ProceduralExecutor::execute_routine`)
//! and is not changed. Everything above is done in the SQL callback the
//! interpreter already hands every statement and every expression to: the
//! callback replaces the record fields and `TG_` names by SQL literals before
//! the statement reaches the engine, and takes the statements that belong to
//! the trigger's world (a record field assignment, `RETURN NEW`, `RAISE`)
//! itself. Local variables keep the interpreter's own rule (`$name`).
//!
//! This module holds no engine handle: the engine side (which triggers fire,
//! on which transaction, what happens to the returned row) is
//! `crate::trigger_runtime`.

use super::procedural::{ExecutionContext, ProceduralBlock, ProceduralExecutor, ProceduralStatement, RaiseLevel};
use crate::{Error, Result, Schema, Value};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};

/// The function name an inline trigger body is planned under:
/// `EXECUTE FUNCTION __hdb_inline_trigger('<dialect>', '<body>')`.
pub(crate) const INLINE_TRIGGER_FUNCTION: &str = "__hdb_inline_trigger";

/// The procedure name `ALTER TRIGGER … RENAME` and `ALTER TABLE … ENABLE |
/// DISABLE TRIGGER` are planned under.
pub(crate) const TRIGGER_ADMIN_PROCEDURE: &str = "__hdb_trigger_admin";

/// How deep trigger firings may nest (a trigger whose function writes to a
/// table whose trigger writes to a table …).
pub const MAX_TRIGGER_NESTING: usize = 64;

/// Stack a chain of nested firings may use, measured from the first firing on
/// the thread. Nesting is recursion through the statement executors, and the
/// threads statements run on have a fixed stack; a chain is refused with the
/// nesting error before it can overflow one.
const MAX_TRIGGER_STACK_BYTES: usize = 5 * 1024 * 1024 / 8;

/// Marks the nesting-limit error (SQLSTATE 54001).
pub const TRIGGER_NESTING_MARKER: &str = "trigger nesting is too deep";

/// Marks a trigger form this build refuses (SQLSTATE 0A000).
pub const TRIGGER_UNSUPPORTED_MARKER: &str = "trigger form is not supported";

/// Marks `RAISE EXCEPTION` (SQLSTATE P0001 unless the message carries one).
pub const RAISE_EXCEPTION_PREFIX: &str = "RAISE EXCEPTION:";

/// Marks a write route that cannot fire the triggers of the table it writes
/// to (SQLSTATE 0A000).
pub const TRIGGER_ROUTE_MARKER: &str = "cannot fire the triggers of table";

/// Marks a `NEW`/`OLD` record used where the event has none (SQLSTATE 55000).
pub const RECORD_NOT_ASSIGNED_MARKER: &str = "is not assigned yet";

/// Marks a trigger function that ends without `RETURN` (SQLSTATE 2F005).
pub const NO_RETURN_MARKER: &str = "control reached end of trigger function without RETURN";

/// What a trigger runs, stored beside its definition (the definition record
/// itself cannot grow a field: it is positional and travels in the log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerExecInfo {
    /// The function named by `EXECUTE FUNCTION`, lower-cased. `None` for an
    /// inline body.
    pub function: Option<String>,
    /// The literal arguments of `EXECUTE FUNCTION f(…)`, as `TG_ARGV` text.
    pub args: Vec<String>,
    /// The inline body of a MySQL or SQLite form trigger.
    pub inline: Option<InlineTriggerBody>,
}

/// An inline trigger body, kept as written and as translated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineTriggerBody {
    /// `mysql` or `sqlite`.
    pub dialect: String,
    /// The body as the user wrote it (for the catalogs and for dumps).
    pub source: String,
    /// The body as a block the interpreter parses.
    pub block: String,
}

/// The operation a trigger fires for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerOp {
    Insert,
    Update,
    Delete,
    Truncate,
}

impl TriggerOp {
    /// `TG_OP`.
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerOp::Insert => "INSERT",
            TriggerOp::Update => "UPDATE",
            TriggerOp::Delete => "DELETE",
            TriggerOp::Truncate => "TRUNCATE",
        }
    }
}

/// One firing: everything the function can see.
pub struct TriggerCall<'a> {
    pub trigger_name: &'a str,
    pub table_name: &'a str,
    pub table_schema: &'a str,
    pub op: TriggerOp,
    /// `BEFORE`, `AFTER` or `INSTEAD OF`.
    pub when: &'static str,
    /// `ROW` or `STATEMENT`.
    pub level: &'static str,
    pub args: &'a [String],
    pub schema: &'a Schema,
    pub new: Option<Vec<Value>>,
    pub old: Option<Vec<Value>>,
    /// An inline body ends with an implicit `RETURN NEW` (`OLD` on DELETE).
    pub implicit_return: bool,
}

/// What the function returned.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerReturn {
    /// `RETURN NULL`.
    Null,
    /// `RETURN NEW`: the row as the function left it, and which columns it assigned.
    New(Vec<Value>, Vec<bool>),
    /// `RETURN OLD`.
    Old,
}

const RETURN_NEW_SENTINEL: &str = "\u{1}helios-trigger-return:NEW";
const RETURN_OLD_SENTINEL: &str = "\u{1}helios-trigger-return:OLD";
const RAISE_STATEMENT_MARK: &str = "\u{1}helios-trigger-raise\u{1f}";

struct CallState<'a> {
    call: TriggerCall<'a>,
    assigned: Vec<bool>,
}

thread_local! {
    static NESTING: Cell<usize> = const { Cell::new(0) };
    static NESTING_BASE: Cell<usize> = const { Cell::new(0) };
}

/// One level of trigger nesting on this thread; released on drop.
pub(crate) struct NestingGuard(());

impl NestingGuard {
    /// Enter one more level, or refuse the firing of `trigger` when the chain
    /// is too deep.
    pub(crate) fn enter(trigger: &str) -> Result<Self> {
        let marker = 0u8;
        let here = std::ptr::addr_of!(marker) as usize;
        let depth = NESTING.with(Cell::get);
        if depth == 0 {
            NESTING_BASE.with(|base| base.set(here));
        }
        let used = NESTING_BASE.with(Cell::get).abs_diff(here);
        if depth >= MAX_TRIGGER_NESTING || used > MAX_TRIGGER_STACK_BYTES {
            return Err(Error::query_execution(format!(
                "{TRIGGER_NESTING_MARKER}: trigger \"{trigger}\" would fire at nesting level {} (the limit is \
                 {MAX_TRIGGER_NESTING} levels, or less when the chain uses up its stack). A trigger whose function \
                 writes to its own table, or two triggers that write to each other's tables, fire without end",
                depth + 1
            )));
        }
        NESTING.with(|n| n.set(depth + 1));
        Ok(Self(()))
    }

    /// The current nesting level on this thread (0 outside any trigger).
    pub(crate) fn depth() -> usize {
        NESTING.with(Cell::get)
    }
}

impl Drop for NestingGuard {
    fn drop(&mut self) {
        NESTING.with(|n| n.set(n.get().saturating_sub(1)));
    }
}

/// Parse a trigger function body and prepare it for [`run_trigger_function`].
pub(crate) fn prepare_block(body: &str) -> Result<ProceduralBlock> {
    let mut parser = super::procedural::ProceduralParser::new(body);
    let mut block = parser
        .parse_block()
        .map_err(|e| Error::query_execution(format!("Failed to parse trigger function body: {e}")))?;
    rewrite_raise(&mut block.statements);
    for handler in &mut block.exception_handlers {
        rewrite_raise(&mut handler.body);
    }
    Ok(block)
}

/// Turn every `RAISE` into a statement the SQL callback takes itself, so the
/// message is formatted against the trigger's world and a notice reaches the
/// caller.
fn rewrite_raise(statements: &mut [ProceduralStatement]) {
    for statement in statements {
        match statement {
            ProceduralStatement::Raise {
                level,
                message,
                sqlstate,
                ..
            } => {
                let raw = match message {
                    Some(crate::sql::LogicalExpr::Literal(Value::String(text))) => text.clone(),
                    _ => String::new(),
                };
                let level = match level {
                    RaiseLevel::Exception => "EXCEPTION",
                    RaiseLevel::Warning => "WARNING",
                    RaiseLevel::Notice => "NOTICE",
                    RaiseLevel::Info => "INFO",
                    RaiseLevel::Log => "LOG",
                    RaiseLevel::Debug => "DEBUG",
                };
                let code = sqlstate.clone().unwrap_or_default();
                *statement = ProceduralStatement::Execute {
                    sql: format!("{RAISE_STATEMENT_MARK}{level}\u{1f}{code}\u{1f}{raw}"),
                    into_variables: Vec::new(),
                };
            }
            ProceduralStatement::If {
                then_block,
                elsif_branches,
                else_block,
                ..
            } => {
                rewrite_raise(then_block);
                for (_, body) in elsif_branches {
                    rewrite_raise(body);
                }
                if let Some(body) = else_block {
                    rewrite_raise(body);
                }
            }
            ProceduralStatement::Case {
                when_branches,
                else_block,
            }
            | ProceduralStatement::SimpleCase {
                when_branches,
                else_block,
                ..
            } => {
                for (_, body) in when_branches {
                    rewrite_raise(body);
                }
                if let Some(body) = else_block {
                    rewrite_raise(body);
                }
            }
            ProceduralStatement::Loop { body, .. }
            | ProceduralStatement::While { body, .. }
            | ProceduralStatement::ForNumeric { body, .. }
            | ProceduralStatement::ForQuery { body, .. } => rewrite_raise(body),
            ProceduralStatement::Block(block) => {
                rewrite_raise(&mut block.statements);
                for handler in &mut block.exception_handlers {
                    rewrite_raise(&mut handler.body);
                }
            }
            _ => {}
        }
    }
}

/// Run one trigger function for one firing.
///
/// `run_sql` executes one SQL statement on the firing transaction and returns
/// its rows; `notice` receives every `RAISE` below `EXCEPTION`.
pub(crate) fn run_trigger_function(
    block: &ProceduralBlock,
    call: TriggerCall<'_>,
    limits: (u64, Option<u64>),
    run_sql: &mut dyn FnMut(&str) -> Result<Vec<Vec<Value>>>,
    notice: &mut dyn FnMut(&str, &str),
) -> Result<TriggerReturn> {
    let implicit_return = call.implicit_return;
    let op = call.op;
    let columns = call.schema.columns.len();
    let state = RefCell::new(CallState {
        call,
        assigned: vec![false; columns],
    });
    let evaluator = crate::sql::Evaluator::new(std::sync::Arc::new(Schema { columns: vec![] }));
    let returned;
    let return_value;
    {
        let callback = |sql: &str| -> Result<Vec<Vec<Value>>> { statement(&state, sql, run_sql, notice) };
        let mut ctx = ExecutionContext::new(&evaluator, callback);
        ctx.max_loop_iterations = limits.0;
        ctx.default_statement_timeout_ms = limits.1;
        ProceduralExecutor::execute_routine(block, &mut ctx)?;
        returned = ctx.returned;
        return_value = ctx.return_value.take();
    }
    let state = state.into_inner();
    let new_row = |state: CallState<'_>| match state.call.new {
        Some(values) => Ok(TriggerReturn::New(values, state.assigned)),
        None => Err(Error::query_execution(format!(
            "record \"new\" {RECORD_NOT_ASSIGNED_MARKER}: a {} trigger has no NEW row to return",
            op.as_str()
        ))),
    };
    if !returned {
        if !implicit_return {
            return Err(Error::query_execution(format!(
                "{NO_RETURN_MARKER} (trigger \"{}\")",
                state.call.trigger_name
            )));
        }
        return if state.call.new.is_some() {
            new_row(state)
        } else {
            Ok(TriggerReturn::Old)
        };
    }
    match return_value {
        None | Some(Value::Null) => Ok(TriggerReturn::Null),
        Some(Value::String(text)) if text == RETURN_NEW_SENTINEL => new_row(state),
        Some(Value::String(text)) if text == RETURN_OLD_SENTINEL => Ok(TriggerReturn::Old),
        Some(_) => Err(Error::query_execution(format!(
            "trigger function of trigger \"{}\" must return NEW, OLD or NULL",
            state.call.trigger_name
        ))),
    }
}

/// One statement or expression from the interpreter.
fn statement(
    state: &RefCell<CallState<'_>>,
    sql: &str,
    run_sql: &mut dyn FnMut(&str) -> Result<Vec<Vec<Value>>>,
    notice: &mut dyn FnMut(&str, &str),
) -> Result<Vec<Vec<Value>>> {
    let trimmed = sql.trim();
    if let Some(rest) = trimmed.strip_prefix(RAISE_STATEMENT_MARK) {
        return raise(state, rest, run_sql, notice);
    }
    if let Some(expr) = strip_select(trimmed) {
        if expr.eq_ignore_ascii_case("NEW") {
            return Ok(vec![vec![Value::String(RETURN_NEW_SENTINEL.to_string())]]);
        }
        if expr.eq_ignore_ascii_case("OLD") {
            return Ok(vec![vec![Value::String(RETURN_OLD_SENTINEL.to_string())]]);
        }
    }
    if let Some((record, column, expr)) = split_record_assignment(trimmed) {
        if !record.eq_ignore_ascii_case("NEW") {
            return Err(Error::query_execution(format!(
                "cannot assign to OLD.{column}: only the NEW row of a trigger can be changed"
            )));
        }
        let query = format!("SELECT {}", substitute(&state.borrow().call, expr)?);
        let value = run_sql(&query)?
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next())
            .unwrap_or(Value::Null);
        let mut guard = state.borrow_mut();
        let index = field_index(guard.call.schema, "new", &column)?;
        let Some(new) = guard.call.new.as_mut() else {
            return Err(Error::query_execution(format!(
                "record \"new\" {RECORD_NOT_ASSIGNED_MARKER}: a {} trigger has no NEW row",
                guard.call.op.as_str()
            )));
        };
        if let Some(slot) = new.get_mut(index) {
            *slot = value;
        }
        if let Some(flag) = guard.assigned.get_mut(index) {
            *flag = true;
        }
        return Ok(Vec::new());
    }
    let substituted = substitute(&state.borrow().call, trimmed)?;
    run_sql(&substituted)
}

/// `RAISE`: format the message, then fail (EXCEPTION) or report (the rest).
fn raise(
    state: &RefCell<CallState<'_>>,
    rest: &str,
    run_sql: &mut dyn FnMut(&str) -> Result<Vec<Vec<Value>>>,
    notice: &mut dyn FnMut(&str, &str),
) -> Result<Vec<Vec<Value>>> {
    let mut parts = rest.splitn(3, '\u{1f}');
    let level = parts.next().unwrap_or("EXCEPTION");
    let code = parts.next().unwrap_or("");
    let raw = parts.next().unwrap_or("").trim();
    let message = if raw.is_empty() {
        String::new()
    } else {
        let query = format!("SELECT {}", substitute(&state.borrow().call, raw)?);
        let row = run_sql(&query)?.into_iter().next().unwrap_or_default();
        format_raise(&row)
    };
    if level == "EXCEPTION" {
        let code = if code.is_empty() {
            String::new()
        } else {
            format!(" [SQLSTATE: {code}]")
        };
        return Err(Error::query_execution(format!(
            "{RAISE_EXCEPTION_PREFIX}{code} {message}"
        )));
    }
    notice(level, &message);
    Ok(Vec::new())
}

/// `RAISE 'a % b', x`: the first value is the format, `%` takes the next
/// value, `%%` is a percent sign.
fn format_raise(row: &[Value]) -> String {
    let mut values = row.iter();
    let format = values.next().map(value_text).unwrap_or_default();
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
                None => out.push('%'),
            }
        }
    }
    out
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "<NULL>".to_string(),
        Value::String(text) => text.clone(),
        other => format!("{other}"),
    }
}

/// `SELECT <expr>` → `<expr>`.
fn strip_select(sql: &str) -> Option<&str> {
    let head = sql.get(..6)?;
    if !head.eq_ignore_ascii_case("SELECT") {
        return None;
    }
    let rest = sql.get(6..)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

/// `NEW.col := expr` / `NEW.col = expr` → (`NEW`, `col`, `expr`).
fn split_record_assignment(sql: &str) -> Option<(&str, String, &str)> {
    let record = sql.get(..3)?;
    if !(record.eq_ignore_ascii_case("NEW") || record.eq_ignore_ascii_case("OLD")) {
        return None;
    }
    let rest = sql.get(3..)?.trim_start();
    let rest = rest.strip_prefix('.')?.trim_start();
    let (column, rest) = take_identifier(rest)?;
    let rest = rest.trim_start();
    let expr = match rest.strip_prefix(":=") {
        Some(expr) => expr,
        None => {
            let expr = rest.strip_prefix('=')?;
            if expr.starts_with('=') || expr.starts_with('>') {
                return None;
            }
            expr
        }
    };
    Some((record, column, expr.trim().trim_end_matches(';').trim()))
}

/// A plain or `"quoted"` identifier at the start of `text`, and what follows it.
fn take_identifier(text: &str) -> Option<(String, &str)> {
    if let Some(rest) = text.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if matches!(chars.peek(), Some((_, '"'))) {
                    chars.next();
                    name.push('"');
                    continue;
                }
                return Some((name, rest.get(i + 1..)?));
            }
            name.push(c);
        }
        return None;
    }
    let end = text
        .char_indices()
        .find(|&(_, c)| !(c.is_alphanumeric() || c == '_'))
        .map_or(text.len(), |(i, _)| i);
    if end == 0 || text.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((text.get(..end)?.to_string(), text.get(end..)?))
}

fn field_index(schema: &Schema, record: &str, column: &str) -> Result<usize> {
    schema
        .columns
        .iter()
        .position(|c| c.name == column)
        .or_else(|| schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(column)))
        .ok_or_else(|| Error::query_execution(format!("record \"{record}\" has no field \"{column}\"")))
}

/// Replace `NEW.<column>`, `OLD.<column>` and the `TG_` names in `sql` by SQL
/// literals. String literals, quoted identifiers, comments and dollar-quoted
/// text are copied through; a name that follows a `.` (a qualified column
/// such as `t.new`) or is called as a function is left alone.
pub(crate) fn substitute(call: &TriggerCall<'_>, sql: &str) -> Result<String> {
    use crate::sql::interpolate::value_to_sql_literal;

    let mut out = String::with_capacity(sql.len() + 16);
    let mut rest = sql;
    let mut after_dot = false;
    while let Some(c) = rest.chars().next() {
        if let Some(len) = quoted_span(rest) {
            out.push_str(rest.get(..len).unwrap_or(rest));
            rest = rest.get(len..).unwrap_or("");
            after_dot = false;
            continue;
        }
        if !(c.is_alphabetic() || c == '_') {
            out.push(c);
            rest = rest.get(c.len_utf8()..).unwrap_or("");
            if !c.is_whitespace() {
                after_dot = c == '.';
            }
            continue;
        }
        let end = rest
            .char_indices()
            .find(|&(_, ch)| !(ch.is_alphanumeric() || ch == '_' || ch == '$'))
            .map_or(rest.len(), |(i, _)| i);
        let word = rest.get(..end).unwrap_or(rest);
        let tail = rest.get(end..).unwrap_or("");
        let qualified = after_dot;
        after_dot = false;
        if qualified {
            out.push_str(word);
            rest = tail;
            continue;
        }
        let is_new = word.eq_ignore_ascii_case("NEW");
        if is_new || word.eq_ignore_ascii_case("OLD") {
            let after = tail.trim_start();
            if let Some(field) = after.strip_prefix('.') {
                let field = field.trim_start();
                let record = if is_new { "new" } else { "old" };
                if field.starts_with('*') {
                    return Err(Error::query_execution(format!(
                        "{}.* in a trigger function: this {TRIGGER_UNSUPPORTED_MARKER}; name the columns",
                        record.to_ascii_uppercase()
                    )));
                }
                if let Some((column, remaining)) = take_identifier(field) {
                    let index = field_index(call.schema, record, &column)?;
                    let values = if is_new { call.new.as_ref() } else { call.old.as_ref() };
                    let Some(values) = values else {
                        return Err(Error::query_execution(format!(
                            "record \"{record}\" {RECORD_NOT_ASSIGNED_MARKER}: a {} {} trigger has no {} row",
                            call.op.as_str(),
                            call.level,
                            record.to_ascii_uppercase()
                        )));
                    };
                    out.push_str(&value_to_sql_literal(values.get(index).unwrap_or(&Value::Null)));
                    rest = remaining;
                    continue;
                }
            }
            out.push_str(word);
            rest = tail;
            continue;
        }
        if tail.trim_start().starts_with('(') {
            out.push_str(word);
            rest = tail;
            continue;
        }
        let text = |s: &str| value_to_sql_literal(&Value::String(s.to_string()));
        let upper = word.to_ascii_uppercase();
        let replacement = match upper.as_str() {
            "TG_OP" => Some(text(call.op.as_str())),
            "TG_NAME" => Some(text(call.trigger_name)),
            "TG_WHEN" => Some(text(call.when)),
            "TG_LEVEL" => Some(text(call.level)),
            "TG_TABLE_NAME" | "TG_RELNAME" => Some(text(call.table_name)),
            "TG_TABLE_SCHEMA" => Some(text(call.table_schema)),
            "TG_NARGS" => Some(call.args.len().to_string()),
            "TG_ARGV" => {
                let after = tail.trim_start();
                let inner = after.strip_prefix('[').and_then(|s| s.split_once(']'));
                match inner {
                    Some((index, remaining)) => {
                        let literal = index
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| call.args.get(i))
                            .map_or_else(|| "NULL".to_string(), |arg| text(arg.as_str()));
                        out.push_str(&literal);
                        rest = remaining;
                        continue;
                    }
                    None => None,
                }
            }
            _ => None,
        };
        match replacement {
            Some(literal) => out.push_str(&literal),
            None => out.push_str(word),
        }
        rest = tail;
    }
    Ok(out)
}

/// The byte length of the quoted span `text` starts with: a string literal, a
/// quoted identifier, a comment or a dollar-quoted string. `None` when `text`
/// starts with none of them. An unterminated span runs to the end.
pub(crate) fn quoted_span(text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    match first {
        '\'' | '"' | '`' => {
            let mut iter = text.char_indices().skip(1).peekable();
            while let Some((i, c)) = iter.next() {
                if c == first {
                    if matches!(iter.peek(), Some(&(_, next)) if next == first) {
                        iter.next();
                        continue;
                    }
                    return Some(i + c.len_utf8());
                }
            }
            Some(text.len())
        }
        '-' if text.starts_with("--") => Some(text.find('\n').map_or(text.len(), |i| i + 1)),
        '/' if text.starts_with("/*") => Some(text.find("*/").map_or(text.len(), |i| i + 2)),
        '$' => {
            let tag_end = text.get(1..)?.find('$')? + 1;
            let tag = text.get(..=tag_end)?;
            let inner = tag.get(1..tag.len().saturating_sub(1))?;
            if inner.starts_with(|c: char| c.is_ascii_digit())
                || !inner.chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                return None;
            }
            let body = text.get(tag.len()..)?;
            Some(body.find(tag).map_or(text.len(), |i| tag.len() + i + tag.len()))
        }
        _ => None,
    }
}

/// The SQLSTATE of an error this module (or the trigger runtime) raised, read
/// from the message; `None` for any other message.
pub fn sqlstate_for_message(message: &str) -> Option<&'static str> {
    if let Some(at) = message.find(RAISE_EXCEPTION_PREFIX) {
        let rest = message
            .get(at + RAISE_EXCEPTION_PREFIX.len()..)
            .unwrap_or("")
            .trim_start();
        let code = rest
            .strip_prefix("[SQLSTATE: ")
            .and_then(|s| s.split_once(']'))
            .map(|(code, _)| code.trim());
        return Some(match code {
            Some(code) if code.len() == 5 && code.chars().all(|c| c.is_ascii_alphanumeric()) => intern_sqlstate(code),
            _ => "P0001",
        });
    }
    if message.contains(TRIGGER_NESTING_MARKER) {
        return Some("54001");
    }
    if message.contains(TRIGGER_UNSUPPORTED_MARKER) || message.contains(TRIGGER_ROUTE_MARKER) {
        return Some("0A000");
    }
    if message.contains(RECORD_NOT_ASSIGNED_MARKER) {
        return Some("55000");
    }
    if message.contains(NO_RETURN_MARKER) {
        return Some("2F005");
    }
    None
}

/// A `'static` copy of a user-chosen SQLSTATE. The set a database's routines
/// raise is small; past a bound the generic P0001 is reported instead of
/// growing the table.
fn intern_sqlstate(code: &str) -> &'static str {
    static CODES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<&'static str>>> =
        std::sync::OnceLock::new();
    let mut codes = CODES
        .get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()))
        .lock();
    let upper = code.to_ascii_uppercase();
    if let Some(known) = codes.get(upper.as_str()) {
        return known;
    }
    if codes.len() >= 4096 {
        return "P0001";
    }
    let leaked: &'static str = Box::leak(upper.into_boxed_str());
    codes.insert(leaked);
    leaked
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{Column, DataType};

    fn schema() -> Schema {
        Schema {
            columns: vec![Column::new("id", DataType::Int4), Column::new("note", DataType::Text)],
        }
    }

    fn call<'a>(schema: &'a Schema, args: &'a [String]) -> TriggerCall<'a> {
        TriggerCall {
            trigger_name: "trg",
            table_name: "t",
            table_schema: "public",
            op: TriggerOp::Update,
            when: "BEFORE",
            level: "ROW",
            args,
            schema,
            new: Some(vec![Value::Int4(2), Value::String("it's".to_string())]),
            old: Some(vec![Value::Int4(1), Value::Null]),
            implicit_return: false,
        }
    }

    #[test]
    fn record_fields_and_tg_names_become_literals() {
        let schema = schema();
        let args = vec!["a".to_string()];
        let call = call(&schema, &args);
        let out = substitute(
            &call,
            "INSERT INTO log VALUES (NEW.id, old.id, NEW.note, OLD.note, TG_OP, TG_ARGV[0], TG_ARGV[3], TG_NARGS)",
        )
        .unwrap();
        assert_eq!(
            out,
            "INSERT INTO log VALUES (2, 1, 'it''s', NULL, 'UPDATE', 'a', NULL, 1)"
        );
    }

    #[test]
    fn quoted_text_qualified_names_and_calls_are_left_alone() {
        let schema = schema();
        let call = call(&schema, &[]);
        let sql = "SELECT 'NEW.id', \"NEW\".id, t.new, x.TG_OP, tg_op(), $q$ NEW.id $q$ -- NEW.id";
        assert_eq!(substitute(&call, sql).unwrap(), sql);
    }

    #[test]
    fn an_unknown_field_and_a_missing_record_are_errors() {
        let schema = schema();
        let mut call = call(&schema, &[]);
        assert!(substitute(&call, "SELECT NEW.nope")
            .unwrap_err()
            .to_string()
            .contains("has no field"));
        call.old = None;
        let err = substitute(&call, "SELECT OLD.id").unwrap_err().to_string();
        assert!(err.contains(RECORD_NOT_ASSIGNED_MARKER), "{err}");
        assert_eq!(sqlstate_for_message(&err), Some("55000"));
    }

    #[test]
    fn assignment_is_recognised_and_comparison_is_not() {
        assert_eq!(
            split_record_assignment("NEW.note := 'x' || NEW.note"),
            Some(("NEW", "note".to_string(), "'x' || NEW.note"))
        );
        assert_eq!(
            split_record_assignment("new . \"Note\" = 1"),
            Some(("new", "Note".to_string(), "1"))
        );
        assert_eq!(split_record_assignment("NEWER.note := 1"), None);
        assert_eq!(split_record_assignment("NEW.note == 1"), None);
    }

    #[test]
    fn raise_formats_percent_placeholders() {
        let row = vec![Value::String("a % b %% c %".to_string()), Value::Int4(7)];
        assert_eq!(format_raise(&row), "a 7 b % c %");
    }

    #[test]
    fn a_function_runs_with_assignment_condition_and_return() {
        let schema = schema();
        let block = prepare_block(
            "BEGIN IF NEW.id > 1 THEN NEW.note := 'big'; END IF; RAISE NOTICE 'id %', NEW.id; RETURN NEW; END",
        )
        .unwrap();
        let mut notices = Vec::new();
        let mut run = |sql: &str| -> Result<Vec<Vec<Value>>> {
            Ok(match sql {
                "SELECT 2 > 1" => vec![vec![Value::Boolean(true)]],
                "SELECT 'big'" => vec![vec![Value::String("big".to_string())]],
                "SELECT 'id %', 2" => vec![vec![Value::String("id %".to_string()), Value::Int4(2)]],
                other => panic!("unexpected statement: {other}"),
            })
        };
        let out = run_trigger_function(
            &block,
            call(&schema, &[]),
            (0, None),
            &mut run,
            &mut |level, message| notices.push(format!("{level}: {message}")),
        )
        .unwrap();
        assert_eq!(
            out,
            TriggerReturn::New(
                vec![Value::Int4(2), Value::String("big".to_string())],
                vec![false, true]
            )
        );
        assert_eq!(notices, vec!["NOTICE: id 2".to_string()]);
    }

    #[test]
    fn raise_exception_carries_its_sqlstate() {
        let schema = schema();
        let block = prepare_block("BEGIN RAISE EXCEPTION SQLSTATE '23514' 'no %', NEW.id; END").unwrap();
        let mut run =
            |_: &str| -> Result<Vec<Vec<Value>>> { Ok(vec![vec![Value::String("no %".into()), Value::Int4(2)]]) };
        let err = run_trigger_function(&block, call(&schema, &[]), (0, None), &mut run, &mut |_, _| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("RAISE EXCEPTION: [SQLSTATE: 23514] no 2"), "{err}");
        assert_eq!(sqlstate_for_message(&err), Some("23514"));
        assert_eq!(sqlstate_for_message("RAISE EXCEPTION: plain"), Some("P0001"));
    }

    #[test]
    fn a_function_without_return_is_refused() {
        let schema = schema();
        let block = prepare_block("BEGIN NULL; END").unwrap();
        let mut run = |_: &str| -> Result<Vec<Vec<Value>>> { Ok(vec![]) };
        let err = run_trigger_function(&block, call(&schema, &[]), (0, None), &mut run, &mut |_, _| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains(NO_RETURN_MARKER), "{err}");
    }

    #[test]
    fn nesting_is_counted_and_released() {
        assert_eq!(NestingGuard::depth(), 0);
        {
            let _a = NestingGuard::enter("a").unwrap();
            let _b = NestingGuard::enter("b").unwrap();
            assert_eq!(NestingGuard::depth(), 2);
        }
        assert_eq!(NestingGuard::depth(), 0);
    }
}
