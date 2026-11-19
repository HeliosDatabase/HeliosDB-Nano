//! Procedural Language Runtime
//!
//! Execution engine for procedural code blocks. Provides variable scoping,
//! control flow execution, and SQL statement execution within procedural context.

use super::ast::*;
use crate::sql::Evaluator;
use crate::{DataType, Error, Result, Value};
use std::collections::HashMap;

/// Maximum recursion depth for procedure calls
pub const MAX_CALL_DEPTH: usize = 100;

/// Variable scope for procedural execution
#[derive(Debug, Clone)]
pub struct VariableScope {
    /// Variables in this scope
    variables: HashMap<String, Variable>,
    /// Parent scope (for nested blocks)
    parent: Option<Box<VariableScope>>,
}

/// A variable with its value and metadata
#[derive(Debug, Clone)]
pub struct Variable {
    pub value: Value,
    pub data_type: Option<DataType>,
    pub is_constant: bool,
    pub not_null: bool,
}

impl VariableScope {
    pub fn new() -> Self {
        Self {
            variables: HashMap::new(),
            parent: None,
        }
    }

    pub fn with_parent(parent: VariableScope) -> Self {
        Self {
            variables: HashMap::new(),
            parent: Some(Box::new(parent)),
        }
    }

    /// Declare a new variable in the current scope
    pub fn declare(&mut self, name: String, var: Variable) -> Result<()> {
        if self.variables.contains_key(&name) {
            return Err(Error::query_execution(format!(
                "Variable '{}' already declared in this scope",
                name
            )));
        }
        self.variables.insert(name, var);
        Ok(())
    }

    /// Get a variable's value (searches parent scopes)
    pub fn get(&self, name: &str) -> Option<&Variable> {
        if let Some(var) = self.variables.get(name) {
            Some(var)
        } else if let Some(ref parent) = self.parent {
            parent.get(name)
        } else {
            None
        }
    }

    /// Set a variable's value (searches parent scopes)
    pub fn set(&mut self, name: &str, value: Value) -> Result<()> {
        if let Some(var) = self.variables.get_mut(name) {
            if var.is_constant {
                return Err(Error::query_execution(format!("Cannot assign to constant '{}'", name)));
            }
            if var.not_null && matches!(value, Value::Null) {
                return Err(Error::query_execution(format!(
                    "Cannot assign NULL to NOT NULL variable '{}'",
                    name
                )));
            }
            var.value = value;
            Ok(())
        } else if let Some(ref mut parent) = self.parent {
            parent.set(name, value)
        } else {
            Err(Error::query_execution(format!("Variable '{}' not declared", name)))
        }
    }
}

impl Default for VariableScope {
    fn default() -> Self {
        Self::new()
    }
}

/// Execution context for procedural code
pub struct ExecutionContext<'a> {
    /// Current variable scope
    pub scope: VariableScope,
    /// Expression evaluator
    pub evaluator: &'a Evaluator,
    /// SQL executor function
    pub sql_executor: Box<dyn FnMut(&str) -> Result<Vec<Vec<Value>>> + 'a>,
    /// Current call depth (for recursion detection)
    pub call_depth: usize,
    /// Whether to exit the current block
    pub exit_requested: bool,
    /// Exit label (for labeled EXIT)
    pub exit_label: Option<String>,
    /// Whether to continue to next iteration
    pub continue_requested: bool,
    /// Continue label (for labeled CONTINUE)
    pub continue_label: Option<String>,
    /// Return value (if RETURN was executed)
    pub return_value: Option<Value>,
    /// Whether RETURN was executed
    pub returned: bool,
    /// Call-site arguments, for `$1..$N` in body SQL. Empty outside a routine call.
    pub positional_params: Vec<Value>,
    /// `[procedural] max_loop_iterations`: the most iterations one loop may run
    /// before the routine fails (0 = unlimited).
    pub max_loop_iterations: u64,
    /// The configured statement timeout (`[storage] statement_timeout_ms`),
    /// used when the session has not `SET statement_timeout`.
    pub default_statement_timeout_ms: Option<u64>,
}

/// What a loop does after one pass over its body.
enum LoopFlow {
    /// Run the next iteration.
    Next,
    /// Leave the loop (EXIT of this loop, or a RETURN / an EXIT or CONTINUE
    /// aimed at an enclosing loop, whose flag is still set).
    Leave,
}

impl<'a> ExecutionContext<'a> {
    pub fn new(evaluator: &'a Evaluator, sql_executor: impl FnMut(&str) -> Result<Vec<Vec<Value>>> + 'a) -> Self {
        Self {
            scope: VariableScope::new(),
            evaluator,
            sql_executor: Box::new(sql_executor),
            call_depth: 0,
            exit_requested: false,
            exit_label: None,
            continue_requested: false,
            continue_label: None,
            return_value: None,
            returned: false,
            positional_params: Vec::new(),
            max_loop_iterations: 0,
            default_statement_timeout_ms: None,
        }
    }

    /// True when statements of the current list must stop running: RETURN,
    /// EXIT or CONTINUE is in flight.
    fn interrupted(&self) -> bool {
        self.returned || self.exit_requested || self.continue_requested
    }

    /// Interpolate `$<name>` from the variable scope and `$N` from the call's arguments
    /// into a body SQL statement, immediately before it is handed to `sql_executor`.
    ///
    /// This is the PL/pgSQL half of the shared placeholder contract; the `LANGUAGE sql`
    /// half lives in `FunctionRegistry::execute_sql_procedure`. Both go through
    /// `crate::sql::interpolate` — see that module before changing either.
    ///
    /// The `$` sigil is mandatory: a bare identifier is left alone and reaches the
    /// planner as a column reference, so a variable can never silently shadow a column.
    /// Name lookup is exact-case, matching `VariableScope`.
    pub fn interpolate(&self, sql: &str) -> String {
        use crate::sql::interpolate::{interpolate_sql_placeholders, value_to_sql_literal, Placeholder};

        interpolate_sql_placeholders(sql, |ph| match ph {
            // `checked_sub` — `$0` must resolve to None, not underflow.
            Placeholder::Positional(n) => n
                .checked_sub(1)
                .and_then(|i| self.positional_params.get(i))
                .map(value_to_sql_literal),
            Placeholder::Named(name) => self.scope.get(name).map(|var| value_to_sql_literal(&var.value)),
        })
    }

    /// Push a new variable scope
    pub fn push_scope(&mut self) {
        let old_scope = std::mem::take(&mut self.scope);
        self.scope = VariableScope::with_parent(old_scope);
    }

    /// Pop the current variable scope
    pub fn pop_scope(&mut self) {
        if let Some(parent) = self.scope.parent.take() {
            self.scope = *parent;
        }
    }
}

/// Procedural code executor
pub struct ProceduralExecutor;

impl ProceduralExecutor {
    /// Run a routine's top-level block, then refuse an EXIT or CONTINUE that
    /// no enclosing loop or block took (an unknown label, or one used outside
    /// any loop) instead of letting it silently end the routine.
    pub fn execute_routine(block: &ProceduralBlock, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        Self::execute_block(block, ctx)?;
        if ctx.exit_requested {
            return Err(Error::query_execution(match ctx.exit_label.as_deref() {
                Some(label) => format!(
                    "EXIT names label \"{}\", which is not an enclosing loop or block",
                    label
                ),
                None => "EXIT cannot be used outside a loop, unless it has a label".to_string(),
            }));
        }
        if ctx.continue_requested {
            return Err(Error::query_execution(match ctx.continue_label.as_deref() {
                Some(label) => format!("CONTINUE names label \"{}\", which is not an enclosing loop", label),
                None => "CONTINUE cannot be used outside a loop".to_string(),
            }));
        }
        Ok(())
    }

    /// Execute a procedural block
    pub fn execute_block(block: &ProceduralBlock, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        // Push new scope for this block
        ctx.push_scope();
        let result = Self::execute_block_in_scope(block, ctx);
        // Pop on every path, so an EXCEPTION handler further out sees its own scope.
        ctx.pop_scope();
        result?;

        // `EXIT <block label>` leaves this block and continues after it.
        if ctx.exit_requested
            && ctx.exit_label.is_some()
            && Self::label_matches(ctx.exit_label.as_deref(), block.label.as_deref())
        {
            ctx.exit_requested = false;
            ctx.exit_label = None;
        }
        Ok(())
    }

    fn execute_block_in_scope(block: &ProceduralBlock, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        // Declare variables
        for decl in &block.declarations {
            let default_value = match decl.default {
                Some(ref expr) => {
                    let value = Self::eval_expr(expr, ctx, "DECLARE default")?;
                    Self::cast_to_declared(ctx, value, decl.data_type.as_ref(), &decl.name)?
                }
                None => Value::Null,
            };
            if decl.not_null && matches!(default_value, Value::Null) {
                return Err(Error::query_execution(format!(
                    "variable \"{}\" is declared NOT NULL, so it needs a non-null default value",
                    decl.name
                )));
            }

            ctx.scope.declare(
                decl.name.clone(),
                Variable {
                    value: default_value,
                    data_type: decl.data_type.clone(),
                    is_constant: decl.is_constant,
                    not_null: decl.not_null,
                },
            )?;
        }

        // Execute statements with exception handling
        match Self::execute_statements(&block.statements, ctx) {
            Ok(()) => Ok(()),
            Err(err) => {
                // Check if any exception handler matches
                if let Some(handler) = Self::find_matching_handler(&block.exception_handlers, &err) {
                    Self::execute_statements(&handler.body, ctx)
                } else {
                    // No handler matched, re-raise the exception
                    Err(err)
                }
            }
        }
    }

    /// Run a statement list, stopping when RETURN, EXIT or CONTINUE is in flight.
    fn execute_statements(statements: &[ProceduralStatement], ctx: &mut ExecutionContext<'_>) -> Result<()> {
        for stmt in statements {
            Self::execute_statement(stmt, ctx)?;
            if ctx.interrupted() {
                break;
            }
        }
        Ok(())
    }

    /// Evaluate a procedural expression (see [`PlExpr`]).
    ///
    /// The text goes through `ExecutionContext::interpolate` (the shared
    /// `$`-sigil scanner — `$name` from the variable scope, `$N` from the
    /// call's arguments) and is then evaluated by the engine as
    /// `SELECT <text>`. This is the one rule RETURN has used since UDFs became
    /// callable; conditions, assignments, loop bounds and DECLARE defaults now
    /// use it too, instead of taking the expression's TEXT as its value. The
    /// `$` sigil stays mandatory, so a bare identifier reaches the planner as
    /// a column reference and errors loudly rather than silently resolving.
    ///
    /// No rows yields NULL, matching `execute_sql_function`'s empty-result
    /// rule and PostgreSQL's `SELECT (SELECT … WHERE false)`.
    pub fn eval_expr(expr: &PlExpr, ctx: &mut ExecutionContext<'_>, what: &str) -> Result<Value> {
        let text = expr.text.trim();
        if text.is_empty() {
            return Ok(Value::Null);
        }
        // Borrow ends before `sql_executor` is borrowed mutably below.
        let sql = format!("SELECT {}", ctx.interpolate(text));
        let results = (ctx.sql_executor)(&sql).map_err(|e| Self::explain_eval_error(what, text, e))?;
        Ok(results
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// An expression that named an unknown column almost always wrote a
    /// variable without its `$` sigil: say so. Any other error (a division by
    /// zero, a RAISE in a called function) passes through unchanged, keeping
    /// its SQLSTATE.
    fn explain_eval_error(what: &str, text: &str, e: Error) -> Error {
        let msg = e.to_string();
        let lower = msg.to_lowercase();
        if lower.contains("column") && (lower.contains("not found") || lower.contains("does not exist")) {
            Error::query_execution(format!(
                "{} could not evaluate `{}`: {}. A routine body evaluates its expressions as SQL, \
                 and the `$` sigil is mandatory — reference a parameter or declared variable as \
                 `$name` or `$1`.",
                what, text, msg
            ))
        } else {
            e
        }
    }

    /// Evaluate a condition: TRUE runs the branch; FALSE and NULL do not; any
    /// other value is an error rather than a silent "false".
    pub fn eval_condition(expr: &PlExpr, ctx: &mut ExecutionContext<'_>, what: &str) -> Result<bool> {
        match Self::eval_expr(expr, ctx, what)? {
            Value::Boolean(b) => Ok(b),
            Value::Null => Ok(false),
            other => Err(Error::query_execution(format!(
                "{} condition `{}` must be boolean, but it evaluated to {} ({})",
                what,
                expr.text.trim(),
                other,
                other.data_type()
            ))),
        }
    }

    /// Cast a value assigned to a variable to the variable's declared type.
    fn cast_to_declared(
        ctx: &ExecutionContext<'_>,
        value: Value,
        data_type: Option<&DataType>,
        name: &str,
    ) -> Result<Value> {
        match data_type {
            Some(dt) if !matches!(value, Value::Null) => ctx.evaluator.cast_value(value, dt).map_err(|e| {
                Error::query_execution(format!(
                    "value assigned to variable \"{}\" does not convert to {}: {}",
                    name, dt, e
                ))
            }),
            _ => Ok(value),
        }
    }

    /// An EXIT/CONTINUE aimed at `requested` applies to a loop or block
    /// labelled `label`: an unlabelled one applies to the innermost loop.
    fn label_matches(requested: Option<&str>, label: Option<&str>) -> bool {
        match requested {
            None => true,
            Some(r) => label.is_some_and(|l| l.eq_ignore_ascii_case(r)),
        }
    }

    /// One pass over a loop body, resolving EXIT/CONTINUE aimed at this loop
    /// and leaving the flags set for one aimed further out.
    fn run_loop_body(
        body: &[ProceduralStatement],
        label: Option<&str>,
        ctx: &mut ExecutionContext<'_>,
    ) -> Result<LoopFlow> {
        Self::execute_statements(body, ctx)?;
        if ctx.returned {
            return Ok(LoopFlow::Leave);
        }
        if ctx.exit_requested {
            if Self::label_matches(ctx.exit_label.as_deref(), label) {
                ctx.exit_requested = false;
                ctx.exit_label = None;
            }
            return Ok(LoopFlow::Leave);
        }
        if ctx.continue_requested {
            if Self::label_matches(ctx.continue_label.as_deref(), label) {
                ctx.continue_requested = false;
                ctx.continue_label = None;
                return Ok(LoopFlow::Next);
            }
            return Ok(LoopFlow::Leave);
        }
        Ok(LoopFlow::Next)
    }

    /// Count one loop iteration against `[procedural] max_loop_iterations`
    /// and the statement timeout, so a runaway loop fails instead of hanging.
    fn loop_tick(ctx: &ExecutionContext<'_>, iterations: &mut u64) -> Result<()> {
        *iterations += 1;
        if ctx.max_loop_iterations > 0 && *iterations > ctx.max_loop_iterations {
            return Err(Error::query_execution(format!(
                "loop exceeded {} iterations ([procedural] max_loop_iterations); \
                 raise the setting or set it to 0 for no limit",
                ctx.max_loop_iterations
            )));
        }
        let timeout_ms = match crate::session_statement_timeout_tls() {
            Some(ms) => (ms > 0).then_some(ms),
            None => ctx.default_statement_timeout_ms,
        };
        if let (Some(ms), Some(started)) = (timeout_ms, crate::statement_started_tls()) {
            if started.elapsed() >= std::time::Duration::from_millis(ms) {
                return Err(Error::query_timeout(format!(
                    "canceling statement due to statement timeout ({} ms) in a routine loop",
                    ms
                )));
            }
        }
        Ok(())
    }

    /// Execute a single statement
    #[allow(clippy::indexing_slicing)]
    // SAFETY: All indexing is guarded by `.is_empty()` and `.len()` checks within each match arm
    pub fn execute_statement(stmt: &ProceduralStatement, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        match stmt {
            ProceduralStatement::Assignment { target, value } => {
                let val = Self::eval_expr(value, ctx, "assignment")?;
                let declared = ctx
                    .scope
                    .get(target)
                    .ok_or_else(|| Error::query_execution(format!("Variable '{}' not declared", target)))?
                    .data_type
                    .clone();
                let val = Self::cast_to_declared(ctx, val, declared.as_ref(), target)?;
                ctx.scope.set(target, val)?;
            }

            ProceduralStatement::If {
                condition,
                then_block,
                elsif_branches,
                else_block,
            } => {
                if Self::eval_condition(condition, ctx, "IF")? {
                    Self::execute_statements(then_block, ctx)?;
                } else {
                    let mut executed = false;
                    for (elsif_cond, elsif_stmts) in elsif_branches {
                        if Self::eval_condition(elsif_cond, ctx, "ELSIF")? {
                            Self::execute_statements(elsif_stmts, ctx)?;
                            executed = true;
                            break;
                        }
                    }
                    if !executed {
                        if let Some(else_stmts) = else_block {
                            Self::execute_statements(else_stmts, ctx)?;
                        }
                    }
                }
            }

            ProceduralStatement::While { label, condition, body } => {
                let mut iterations = 0u64;
                while Self::eval_condition(condition, ctx, "WHILE")? {
                    Self::loop_tick(ctx, &mut iterations)?;
                    if let LoopFlow::Leave = Self::run_loop_body(body, label.as_deref(), ctx)? {
                        break;
                    }
                }
            }

            ProceduralStatement::Loop { label, body } => {
                let mut iterations = 0u64;
                loop {
                    Self::loop_tick(ctx, &mut iterations)?;
                    if let LoopFlow::Leave = Self::run_loop_body(body, label.as_deref(), ctx)? {
                        break;
                    }
                }
            }

            ProceduralStatement::ForNumeric {
                label,
                variable,
                lower_bound,
                upper_bound,
                step,
                reverse,
                body,
            } => {
                let first = Self::eval_expr(lower_bound, ctx, "FOR lower bound")?;
                let last = Self::eval_expr(upper_bound, ctx, "FOR upper bound")?;
                let step_val = match step {
                    Some(s) => Self::eval_expr(s, ctx, "FOR BY")?,
                    None => Value::Int8(1),
                };
                if matches!(first, Value::Null) || matches!(last, Value::Null) {
                    return Err(Error::query_execution(
                        "lower and upper bounds of FOR loop cannot be null",
                    ));
                }
                if matches!(step_val, Value::Null) {
                    return Err(Error::query_execution("BY value of FOR loop cannot be null"));
                }
                let first = value_to_i64(&first)?;
                let last = value_to_i64(&last)?;
                let step = value_to_i64(&step_val)?;
                if step <= 0 {
                    return Err(Error::query_execution("BY value of FOR loop must be greater than zero"));
                }
                // PostgreSQL's loop variable is an integer local to the loop.
                let fits_int4 = i32::try_from(first).is_ok() && i32::try_from(last).is_ok();
                let as_value = |i: i64| -> Value {
                    match i32::try_from(i) {
                        Ok(v) if fits_int4 => Value::Int4(v),
                        _ => Value::Int8(i),
                    }
                };

                ctx.push_scope();
                let result = (|| -> Result<()> {
                    ctx.scope.declare(
                        variable.clone(),
                        Variable {
                            value: as_value(first),
                            data_type: None,
                            is_constant: false,
                            not_null: false,
                        },
                    )?;
                    let mut iterations = 0u64;
                    let mut i = first;
                    // `REVERSE first .. last` counts DOWN from `first` (PostgreSQL).
                    while if *reverse { i >= last } else { i <= last } {
                        Self::loop_tick(ctx, &mut iterations)?;
                        ctx.scope.set(variable, as_value(i))?;
                        if let LoopFlow::Leave = Self::run_loop_body(body, label.as_deref(), ctx)? {
                            break;
                        }
                        let next = if *reverse {
                            i.checked_sub(step)
                        } else {
                            i.checked_add(step)
                        };
                        match next {
                            Some(n) => i = n,
                            None => break,
                        }
                    }
                    Ok(())
                })();
                ctx.pop_scope();
                result?;
            }

            ProceduralStatement::Exit { label, when_condition } => {
                let should_exit = match when_condition {
                    Some(cond) => Self::eval_condition(cond, ctx, "EXIT WHEN")?,
                    None => true,
                };
                if should_exit {
                    ctx.exit_requested = true;
                    ctx.exit_label = label.clone();
                }
            }

            ProceduralStatement::Continue { label, when_condition } => {
                let should_continue = match when_condition {
                    Some(cond) => Self::eval_condition(cond, ctx, "CONTINUE WHEN")?,
                    None => true,
                };
                if should_continue {
                    ctx.continue_requested = true;
                    ctx.continue_label = label.clone();
                }
            }

            ProceduralStatement::Return { value } => {
                ctx.return_value = match value {
                    Some(expr) => Some(Self::eval_expr(expr, ctx, "RETURN")?),
                    None => None,
                };
                ctx.returned = true;
            }

            ProceduralStatement::Execute { sql, into_variables } => {
                // Compute the interpolated text BEFORE borrowing `sql_executor` mutably.
                let sql = ctx.interpolate(sql);
                let results = (ctx.sql_executor)(&sql)?;
                if !into_variables.is_empty() && !results.is_empty() && !results[0].is_empty() {
                    for (i, var_name) in into_variables.iter().enumerate() {
                        if i < results[0].len() {
                            ctx.scope.set(var_name, results[0][i].clone())?;
                        }
                    }
                }
            }

            ProceduralStatement::Raise {
                level,
                message,
                sqlstate,
                detail: _,
                hint: _,
            } => {
                let msg = if let Some(expr) = message {
                    let val = ctx.evaluator.evaluate(expr, &crate::Tuple::new(vec![]))?;
                    format!("{}", val)
                } else {
                    String::new()
                };

                match level {
                    RaiseLevel::Exception => {
                        let sqlstate_str = sqlstate
                            .as_ref()
                            .map(|s| format!(" [SQLSTATE: {}]", s))
                            .unwrap_or_default();
                        return Err(Error::query_execution(format!(
                            "RAISE EXCEPTION:{} {}",
                            sqlstate_str, msg
                        )));
                    }
                    // Log at appropriate tracing level based on RAISE level
                    RaiseLevel::Warning => tracing::warn!("[RAISE WARNING] {}", msg),
                    RaiseLevel::Notice => tracing::info!("[RAISE NOTICE] {}", msg),
                    RaiseLevel::Info => tracing::info!("[RAISE INFO] {}", msg),
                    RaiseLevel::Log => tracing::debug!("[RAISE LOG] {}", msg),
                    RaiseLevel::Debug => tracing::trace!("[RAISE DEBUG] {}", msg),
                }
            }

            ProceduralStatement::Block(inner_block) => {
                Self::execute_block(inner_block, ctx)?;
            }

            ProceduralStatement::Null => {
                // No-op
            }

            ProceduralStatement::Print { message } => {
                let val = ctx.evaluator.evaluate(message, &crate::Tuple::new(vec![]))?;
                println!("{}", val);
            }

            ProceduralStatement::Case {
                when_branches,
                else_block,
            } => {
                // Searched CASE: the first true condition runs.
                let mut executed = false;
                for (condition, statements) in when_branches {
                    if Self::eval_condition(condition, ctx, "CASE WHEN")? {
                        Self::execute_statements(statements, ctx)?;
                        executed = true;
                        break;
                    }
                }
                if !executed {
                    Self::run_case_else(else_block.as_deref(), ctx)?;
                }
            }

            ProceduralStatement::SimpleCase {
                operand,
                when_branches,
                else_block,
            } => {
                // Simple CASE: the operand is evaluated once and compared with
                // each WHEN list by SQL equality (`operand IN (values)`), so a
                // NULL operand matches nothing, as in PostgreSQL.
                let operand_val = Self::eval_expr(operand, ctx, "CASE")?;
                let operand_sql = crate::sql::interpolate::value_to_sql_literal(&operand_val);
                let mut executed = false;
                for (when_vals, statements) in when_branches {
                    let test = PlExpr::new(format!("({}) IN ({})", operand_sql, when_vals.text));
                    if Self::eval_condition(&test, ctx, "CASE WHEN")? {
                        Self::execute_statements(statements, ctx)?;
                        executed = true;
                        break;
                    }
                }
                if !executed {
                    Self::run_case_else(else_block.as_deref(), ctx)?;
                }
            }

            ProceduralStatement::SelectInto { query, variables } => {
                // Execute query and store first row's columns into variables
                let query = ctx.interpolate(query);
                let results = (ctx.sql_executor)(&query)?;
                if !results.is_empty() && !results[0].is_empty() {
                    for (i, var_name) in variables.iter().enumerate() {
                        if i < results[0].len() {
                            ctx.scope.set(var_name, results[0][i].clone())?;
                        }
                    }
                }
            }

            ProceduralStatement::ExecuteDynamic {
                sql_expression,
                into_variables,
                using_parameters: _,
            } => {
                // Evaluate the SQL expression to get the SQL string
                let sql_val = ctx.evaluator.evaluate(sql_expression, &crate::Tuple::new(vec![]))?;
                let sql = match sql_val {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                // Execute the dynamic SQL
                let results = (ctx.sql_executor)(&sql)?;
                if !into_variables.is_empty() && !results.is_empty() && !results[0].is_empty() {
                    for (i, var_name) in into_variables.iter().enumerate() {
                        if i < results[0].len() {
                            ctx.scope.set(var_name, results[0][i].clone())?;
                        }
                    }
                }
            }

            // Cursor operations require external cursor infrastructure
            ProceduralStatement::OpenCursor { cursor_name, .. } => {
                return Err(Error::query_execution(format!(
                    "Cursor operations not supported in embedded mode: OPEN {}",
                    cursor_name
                )));
            }
            ProceduralStatement::FetchCursor { cursor_name, .. } => {
                return Err(Error::query_execution(format!(
                    "Cursor operations not supported in embedded mode: FETCH {}",
                    cursor_name
                )));
            }
            ProceduralStatement::CloseCursor { cursor_name } => {
                return Err(Error::query_execution(format!(
                    "Cursor operations not supported in embedded mode: CLOSE {}",
                    cursor_name
                )));
            }

            // Set-returning function features require special result handling
            ProceduralStatement::ReturnNext { .. } => {
                return Err(Error::query_execution(
                    "RETURN NEXT requires set-returning function context",
                ));
            }
            ProceduralStatement::ReturnQuery { .. } => {
                return Err(Error::query_execution(
                    "RETURN QUERY requires set-returning function context",
                ));
            }

            ProceduralStatement::ForQuery {
                label,
                record_variable,
                query,
                body,
            } => {
                // The query runs once; the body runs once per row. Until record
                // variables exist, the loop variable receives the row's first
                // column. A variable declared outside the loop keeps the last
                // row's value afterwards (PostgreSQL); an undeclared one is
                // local to the loop.
                let query = ctx.interpolate(query);
                let rows = (ctx.sql_executor)(&query)?;
                let declared = ctx.scope.get(record_variable).map(|v| v.data_type.clone());
                ctx.push_scope();
                let result = (|| -> Result<()> {
                    if declared.is_none() {
                        ctx.scope.declare(
                            record_variable.clone(),
                            Variable {
                                value: Value::Null,
                                data_type: None,
                                is_constant: false,
                                not_null: false,
                            },
                        )?;
                    }
                    let mut iterations = 0u64;
                    for row in rows {
                        Self::loop_tick(ctx, &mut iterations)?;
                        let value = row.into_iter().next().unwrap_or(Value::Null);
                        let value = Self::cast_to_declared(
                            ctx,
                            value,
                            declared.as_ref().and_then(|d| d.as_ref()),
                            record_variable,
                        )?;
                        ctx.scope.set(record_variable, value)?;
                        if let LoopFlow::Leave = Self::run_loop_body(body, label.as_deref(), ctx)? {
                            break;
                        }
                    }
                    Ok(())
                })();
                ctx.pop_scope();
                result?;
            }

            ProceduralStatement::Call {
                procedure_name,
                arguments: _,
            } => {
                return Err(Error::query_execution(format!(
                    "Procedure calls not supported in embedded mode: CALL {}",
                    procedure_name
                )));
            }
        }

        Ok(())
    }

    /// The ELSE of a CASE statement; with no ELSE and no matching WHEN,
    /// PostgreSQL raises `case not found` (SQLSTATE 20000).
    fn run_case_else(else_block: Option<&[ProceduralStatement]>, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        match else_block {
            Some(stmts) => Self::execute_statements(stmts, ctx),
            None => Err(Error::query_execution(
                "case not found: no WHEN branch of the CASE statement matched and it has no ELSE",
            )),
        }
    }

    /// Find a matching exception handler for an error
    ///
    /// Checks each handler's conditions against the error. Conditions are checked in order:
    /// - Named conditions match against PostgreSQL exception names (e.g., "division_by_zero")
    /// - SQLSTATE conditions match against the SQLSTATE code in the error
    /// - OTHERS matches any exception
    #[allow(clippy::indexing_slicing)]
    // SAFETY: String slicing is guarded by `.find()` returning valid byte offsets
    fn find_matching_handler<'a>(handlers: &'a [ExceptionHandler], error: &Error) -> Option<&'a ExceptionHandler> {
        let error_msg = error.to_string();

        // Extract SQLSTATE from error message if present
        // Format: "RAISE EXCEPTION: [SQLSTATE: XXXXX] message"
        let sqlstate = if let Some(start) = error_msg.find("[SQLSTATE:") {
            let rest = &error_msg[start + 10..];
            if let Some(end) = rest.find(']') {
                Some(rest[..end].trim().to_string())
            } else {
                None
            }
        } else {
            None
        };

        // Map common error patterns to PostgreSQL exception names
        let exception_name = Self::error_to_exception_name(&error_msg);

        for handler in handlers {
            for condition in &handler.conditions {
                let matches = match condition {
                    ExceptionCondition::Others => true,
                    ExceptionCondition::SqlState(state) => sqlstate.as_ref().map(|s| s == state).unwrap_or(false),
                    ExceptionCondition::Named(name) => {
                        let name_upper = name.to_uppercase();
                        exception_name.as_ref().map(|n| n == &name_upper).unwrap_or(false)
                    }
                };

                if matches {
                    return Some(handler);
                }
            }
        }

        None
    }

    /// Map error message patterns to PostgreSQL exception names
    fn error_to_exception_name(error_msg: &str) -> Option<String> {
        let msg_lower = error_msg.to_lowercase();

        // Common PostgreSQL exception name mappings
        if msg_lower.contains("division by zero") || msg_lower.contains("divide by zero") {
            Some("DIVISION_BY_ZERO".to_string())
        } else if msg_lower.contains("null value") && msg_lower.contains("not allowed") {
            Some("NOT_NULL_VIOLATION".to_string())
        } else if msg_lower.contains("unique") && msg_lower.contains("violation") {
            Some("UNIQUE_VIOLATION".to_string())
        } else if msg_lower.contains("foreign key") && msg_lower.contains("violation") {
            Some("FOREIGN_KEY_VIOLATION".to_string())
        } else if msg_lower.contains("no data") || msg_lower.contains("not found") {
            Some("NO_DATA_FOUND".to_string())
        } else if msg_lower.contains("too many rows") {
            Some("TOO_MANY_ROWS".to_string())
        } else if msg_lower.contains("raise exception") {
            Some("RAISE_EXCEPTION".to_string())
        } else {
            None
        }
    }
}

/// Convert a Value to i64 for loop iteration
fn value_to_i64(value: &Value) -> Result<i64> {
    match value {
        Value::Int2(v) => Ok(*v as i64),
        Value::Int4(v) => Ok(*v as i64),
        Value::Int8(v) => Ok(*v),
        Value::Float4(v) => Ok(*v as i64),
        Value::Float8(v) => Ok(*v as i64),
        _ => Err(Error::type_conversion(format!(
            "Cannot convert {} to integer for loop iteration",
            value.data_type()
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Schema;
    use std::sync::Arc;

    #[test]
    fn test_variable_scope() {
        let mut scope = VariableScope::new();

        scope
            .declare(
                "x".to_string(),
                Variable {
                    value: Value::Int4(10),
                    data_type: Some(DataType::Int4),
                    is_constant: false,
                    not_null: false,
                },
            )
            .unwrap();

        assert_eq!(scope.get("x").unwrap().value, Value::Int4(10));

        scope.set("x", Value::Int4(20)).unwrap();
        assert_eq!(scope.get("x").unwrap().value, Value::Int4(20));
    }

    #[test]
    fn test_constant_assignment() {
        let mut scope = VariableScope::new();

        scope
            .declare(
                "PI".to_string(),
                Variable {
                    value: Value::Float8(3.14159),
                    data_type: Some(DataType::Float8),
                    is_constant: true,
                    not_null: false,
                },
            )
            .unwrap();

        // Should fail
        let result = scope.set("PI", Value::Float8(3.0));
        assert!(result.is_err());
    }

    #[test]
    fn test_nested_scope() {
        let mut parent = VariableScope::new();
        parent
            .declare(
                "outer".to_string(),
                Variable {
                    value: Value::Int4(1),
                    data_type: Some(DataType::Int4),
                    is_constant: false,
                    not_null: false,
                },
            )
            .unwrap();

        let child = VariableScope::with_parent(parent);

        // Should find variable from parent
        assert!(child.get("outer").is_some());
    }
}
