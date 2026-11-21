//! Procedural Language Runtime
//!
//! Execution engine for procedural code blocks. Provides variable scoping,
//! control flow execution, and SQL statement execution within procedural context.

use super::ast::*;
use crate::sql::Evaluator;
use crate::{DataType, Error, Result, Value};
use std::collections::HashMap;

mod pg_ext; // bare names bound as parameters, FOUND, records, RAISE, EXCEPTION
pub(crate) use pg_ext::BLOCK_SAVEPOINT_PREFIX;

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

    /// The variable `name` means, with its declared spelling: the exact
    /// spelling first, then (unless `exact`, for a quoted name) any spelling
    /// that differs in case only, innermost scope first.
    pub fn find(&self, name: &str, exact: bool) -> Option<(&str, &Variable)> {
        if let Some((declared, var)) = self.variables.get_key_value(name) {
            return Some((declared.as_str(), var));
        }
        if !exact {
            if let Some((declared, var)) = self.variables.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
                return Some((declared.as_str(), var));
            }
        }
        self.parent.as_ref().and_then(|parent| parent.find(name, exact))
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
    /// Labels in scope, the row count, records and the error being handled.
    pub(crate) pg: pg_ext::PgState,
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
            pg: pg_ext::PgState::default(),
        }
    }

    /// Name the routine being run, so its body can write `routine.variable`.
    pub fn set_routine_name(&mut self, name: &str) {
        let bare = name.rsplit('.').next().unwrap_or(name);
        self.pg.routine_name = Some(bare.to_lowercase());
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
        pg_ext::begin_routine(ctx);
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
        let labelled = pg_ext::enter_label(ctx, block.label.as_deref());
        let result = Self::execute_block_in_scope(block, ctx);
        pg_ext::leave_label(ctx, labelled);
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
            pg_ext::forget_record(ctx, &decl.name);
        }

        if block.exception_handlers.is_empty() {
            return Self::execute_statements(&block.statements, ctx);
        }
        // A block with an EXCEPTION clause is undone to its start when one of
        // its handlers takes an error.
        let mark = pg_ext::mark_block(ctx);
        match Self::execute_statements(&block.statements, ctx) {
            Ok(()) => {
                pg_ext::release_block(ctx, mark);
                Ok(())
            }
            Err(err) => match pg_ext::matching_handler(&block.exception_handlers, &err) {
                Some(handler) => pg_ext::run_handler(ctx, mark, handler, &err),
                None => {
                    pg_ext::release_block(ctx, mark);
                    Err(err)
                }
            },
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
        if expr.text == ROW_COUNT_EXPR {
            return Ok(pg_ext::row_count(ctx));
        }
        // Variables the expression names are bound as parameters; the count
        // and FOUND of the routine's last statement are not this read's.
        let sql = format!("SELECT {text}");
        let results = pg_ext::run_sql(ctx, &sql)
            .map_err(|e| Self::explain_eval_error(what, text, e))?
            .rows;
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
                "{} could not evaluate `{}`: {}. A routine body evaluates its expressions as SQL: \
                 a name that is not a parameter or a declared variable in scope is read as a column.",
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
                // The name as declared: an unquoted name matches in any case.
                let target = &pg_ext::declared_name(ctx, target).unwrap_or_else(|| target.clone());
                let declared = ctx
                    .scope
                    .get(target)
                    .ok_or_else(|| Error::query_execution(format!("Variable '{}' not declared", target)))?
                    .data_type
                    .clone();
                let val = Self::cast_to_declared(ctx, val, declared.as_ref(), target)?;
                pg_ext::forget_record(ctx, target);
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
                    if matches!(Self::run_loop_body(body, label.as_deref(), ctx)?, LoopFlow::Leave) {
                        break;
                    }
                }
            }

            ProceduralStatement::Loop { label, body } => {
                let mut iterations = 0u64;
                loop {
                    Self::loop_tick(ctx, &mut iterations)?;
                    if matches!(Self::run_loop_body(body, label.as_deref(), ctx)?, LoopFlow::Leave) {
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
                        if matches!(Self::run_loop_body(body, label.as_deref(), ctx)?, LoopFlow::Leave) {
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
                // A statement of the routine, or a DML statement with
                // `RETURNING … INTO`. It sets the row count and FOUND.
                let outcome = pg_ext::run_sql(ctx, sql)?;
                pg_ext::set_row_count(ctx, outcome.row_count);
                let found = outcome.row_count > 0;
                if !into_variables.is_empty() {
                    pg_ext::assign_into(ctx, into_variables, outcome, false)?;
                }
                pg_ext::set_found(ctx, found);
            }

            ProceduralStatement::Raise {
                level,
                condition,
                format,
                args,
                options,
            } => {
                pg_ext::raise(ctx, *level, condition.as_deref(), format.as_deref(), args, options)?;
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

            ProceduralStatement::SelectInto {
                query,
                variables,
                strict,
            } => {
                // The first row goes into the targets (NULLs when there is
                // none); STRICT wants exactly one row.
                let outcome = pg_ext::run_sql(ctx, query)?;
                let found = pg_ext::assign_into(ctx, variables, outcome, *strict)?;
                pg_ext::set_row_count(ctx, u64::from(found));
                pg_ext::set_found(ctx, found);
            }

            ProceduralStatement::ExecuteDynamic {
                sql_expression,
                into_variables,
                using_parameters,
                strict,
            } => {
                // The command is computed, then run with the USING values as
                // its parameters: a value is never part of the command text
                // unless the routine itself concatenates it in.
                let command = match Self::eval_expr(sql_expression, ctx, "EXECUTE")? {
                    Value::String(command) => command,
                    Value::Null => {
                        return Err(Error::sql_state(
                            "22004",
                            "query string argument of EXECUTE is null",
                        ))
                    }
                    other => {
                        return Err(Error::sql_state(
                            "42804",
                            format!("the command of EXECUTE must be text, not {}", other.data_type()),
                        ))
                    }
                };
                let mut params = Vec::with_capacity(using_parameters.len());
                for value in using_parameters {
                    params.push(Self::eval_expr(value, ctx, "EXECUTE … USING")?);
                }
                let outcome = pg_ext::run_dynamic(ctx, &command, params)?;
                pg_ext::set_row_count(ctx, outcome.row_count);
                if !into_variables.is_empty() {
                    pg_ext::assign_into(ctx, into_variables, outcome, *strict)?;
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
                // The query runs once; the body runs once per row. A loop
                // variable with a scalar type receives the row's first column;
                // any other holds the row. A variable declared outside the loop keeps the last
                // row's value afterwards (PostgreSQL); an undeclared one is
                // local to the loop.
                let outcome = pg_ext::run_sql(ctx, query)?;
                let columns = outcome.columns;
                let rows = outcome.rows;
                let mut looped = false;
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
                        looped = true;
                        // A loop variable without a scalar type holds the row:
                        // `r.column` reads its fields.
                        pg_ext::set_loop_row(
                            ctx,
                            record_variable,
                            &columns,
                            row,
                            declared.as_ref().and_then(|d| d.as_ref()),
                        )?;
                        if matches!(Self::run_loop_body(body, label.as_deref(), ctx)?, LoopFlow::Leave) {
                            break;
                        }
                    }
                    Ok(())
                })();
                ctx.pop_scope();
                result?;
                pg_ext::set_found(ctx, looped);
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
