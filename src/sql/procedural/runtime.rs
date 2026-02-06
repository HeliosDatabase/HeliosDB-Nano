//! Procedural Language Runtime
//!
//! Execution engine for procedural code blocks. Provides variable scoping,
//! control flow execution, and SQL statement execution within procedural context.

use std::collections::HashMap;
use crate::{Result, Error, Value, DataType};
use crate::sql::Evaluator;
use super::ast::*;

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
                return Err(Error::query_execution(format!(
                    "Cannot assign to constant '{}'",
                    name
                )));
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
            Err(Error::query_execution(format!(
                "Variable '{}' not declared",
                name
            )))
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
}

impl<'a> ExecutionContext<'a> {
    pub fn new(
        evaluator: &'a Evaluator,
        sql_executor: impl FnMut(&str) -> Result<Vec<Vec<Value>>> + 'a,
    ) -> Self {
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
        }
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
    /// Execute a procedural block
    #[allow(clippy::indexing_slicing)]
    // SAFETY: All indexing is guarded by `.is_empty()` and `.len()` checks
    pub fn execute_block<'a>(
        block: &ProceduralBlock,
        ctx: &mut ExecutionContext<'a>,
    ) -> Result<()> {
        // Push new scope for this block
        ctx.push_scope();

        // Declare variables
        for decl in &block.declarations {
            let default_value = if let Some(ref expr) = decl.default {
                ctx.evaluator.evaluate(expr, &crate::Tuple::new(vec![]))?
            } else {
                Value::Null
            };

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
        let execution_result: Result<()> = (|| {
            for stmt in &block.statements {
                Self::execute_statement(stmt, ctx)?;

                // Check for control flow changes
                if ctx.returned || ctx.exit_requested {
                    break;
                }
            }
            Ok(())
        })();

        // Handle exception if one occurred
        match execution_result {
            Ok(()) => {}
            Err(err) => {
                // Check if any exception handler matches
                if let Some(handler) = Self::find_matching_handler(&block.exception_handlers, &err) {
                    // Execute the handler's body
                    for stmt in &handler.body {
                        Self::execute_statement(stmt, ctx)?;
                        if ctx.returned || ctx.exit_requested {
                            break;
                        }
                    }
                } else {
                    // No handler matched, re-raise the exception
                    // Pop scope before returning error
                    ctx.pop_scope();
                    return Err(err);
                }
            }
        }

        // Pop scope
        ctx.pop_scope();

        Ok(())
    }

    /// Execute a single statement
    #[allow(clippy::indexing_slicing)]
    // SAFETY: All indexing is guarded by `.is_empty()` and `.len()` checks within each match arm
    pub fn execute_statement<'a>(
        stmt: &ProceduralStatement,
        ctx: &mut ExecutionContext<'a>,
    ) -> Result<()> {
        match stmt {
            ProceduralStatement::Assignment { target, value } => {
                let val = ctx.evaluator.evaluate(value, &crate::Tuple::new(vec![]))?;
                ctx.scope.set(target, val)?;
            }

            ProceduralStatement::If { condition, then_block, elsif_branches, else_block } => {
                let cond_val = ctx.evaluator.evaluate(condition, &crate::Tuple::new(vec![]))?;
                let cond_bool = match cond_val {
                    Value::Boolean(b) => b,
                    _ => false,
                };

                if cond_bool {
                    for stmt in then_block {
                        Self::execute_statement(stmt, ctx)?;
                        if ctx.returned || ctx.exit_requested {
                            break;
                        }
                    }
                } else {
                    let mut executed = false;
                    for (elsif_cond, elsif_stmts) in elsif_branches {
                        let elsif_val = ctx.evaluator.evaluate(elsif_cond, &crate::Tuple::new(vec![]))?;
                        if matches!(elsif_val, Value::Boolean(true)) {
                            for stmt in elsif_stmts {
                                Self::execute_statement(stmt, ctx)?;
                                if ctx.returned || ctx.exit_requested {
                                    break;
                                }
                            }
                            executed = true;
                            break;
                        }
                    }

                    if !executed {
                        if let Some(else_stmts) = else_block {
                            for stmt in else_stmts {
                                Self::execute_statement(stmt, ctx)?;
                                if ctx.returned || ctx.exit_requested {
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            ProceduralStatement::While { label, condition, body } => {
                loop {
                    let cond_val = ctx.evaluator.evaluate(condition, &crate::Tuple::new(vec![]))?;
                    if !matches!(cond_val, Value::Boolean(true)) {
                        break;
                    }

                    for stmt in body {
                        Self::execute_statement(stmt, ctx)?;

                        if ctx.continue_requested {
                            if ctx.continue_label.is_none() || ctx.continue_label.as_ref() == label.as_ref() {
                                ctx.continue_requested = false;
                                ctx.continue_label = None;
                                break;
                            }
                        }

                        if ctx.exit_requested {
                            if ctx.exit_label.is_none() || ctx.exit_label.as_ref() == label.as_ref() {
                                ctx.exit_requested = false;
                                ctx.exit_label = None;
                                return Ok(());
                            }
                            return Ok(());
                        }

                        if ctx.returned {
                            return Ok(());
                        }
                    }
                }
            }

            ProceduralStatement::Loop { label, body } => {
                loop {
                    for stmt in body {
                        Self::execute_statement(stmt, ctx)?;

                        if ctx.continue_requested {
                            if ctx.continue_label.is_none() || ctx.continue_label.as_ref() == label.as_ref() {
                                ctx.continue_requested = false;
                                ctx.continue_label = None;
                                break;
                            }
                        }

                        if ctx.exit_requested {
                            if ctx.exit_label.is_none() || ctx.exit_label.as_ref() == label.as_ref() {
                                ctx.exit_requested = false;
                                ctx.exit_label = None;
                                return Ok(());
                            }
                            return Ok(());
                        }

                        if ctx.returned {
                            return Ok(());
                        }
                    }
                }
            }

            ProceduralStatement::ForNumeric { label, variable, lower_bound, upper_bound, step, reverse, body } => {
                let lower = ctx.evaluator.evaluate(lower_bound, &crate::Tuple::new(vec![]))?;
                let upper = ctx.evaluator.evaluate(upper_bound, &crate::Tuple::new(vec![]))?;
                let step_val = if let Some(s) = step {
                    ctx.evaluator.evaluate(s, &crate::Tuple::new(vec![]))?
                } else {
                    Value::Int8(1)
                };

                // Convert to i64 for iteration
                let lower_i64 = value_to_i64(&lower)?;
                let upper_i64 = value_to_i64(&upper)?;
                let step_i64 = value_to_i64(&step_val)?.max(1);

                // Create iterator variable
                ctx.scope.declare(
                    variable.clone(),
                    Variable {
                        value: Value::Int8(lower_i64),
                        data_type: Some(DataType::Int8),
                        is_constant: false,
                        not_null: false,
                    },
                )?;

                let range: Vec<i64> = if *reverse {
                    (lower_i64..=upper_i64).rev().step_by(step_i64 as usize).collect()
                } else {
                    (lower_i64..=upper_i64).step_by(step_i64 as usize).collect()
                };

                for i in range {
                    ctx.scope.set(variable, Value::Int8(i))?;

                    for stmt in body {
                        Self::execute_statement(stmt, ctx)?;

                        if ctx.continue_requested {
                            if ctx.continue_label.is_none() || ctx.continue_label.as_ref() == label.as_ref() {
                                ctx.continue_requested = false;
                                ctx.continue_label = None;
                                break;
                            }
                        }

                        if ctx.exit_requested {
                            if ctx.exit_label.is_none() || ctx.exit_label.as_ref() == label.as_ref() {
                                ctx.exit_requested = false;
                                ctx.exit_label = None;
                                return Ok(());
                            }
                            return Ok(());
                        }

                        if ctx.returned {
                            return Ok(());
                        }
                    }
                }
            }

            ProceduralStatement::Exit { label, when_condition } => {
                let should_exit = if let Some(cond) = when_condition {
                    let val = ctx.evaluator.evaluate(cond, &crate::Tuple::new(vec![]))?;
                    matches!(val, Value::Boolean(true))
                } else {
                    true
                };

                if should_exit {
                    ctx.exit_requested = true;
                    ctx.exit_label = label.clone();
                }
            }

            ProceduralStatement::Continue { label, when_condition } => {
                let should_continue = if let Some(cond) = when_condition {
                    let val = ctx.evaluator.evaluate(cond, &crate::Tuple::new(vec![]))?;
                    matches!(val, Value::Boolean(true))
                } else {
                    true
                };

                if should_continue {
                    ctx.continue_requested = true;
                    ctx.continue_label = label.clone();
                }
            }

            ProceduralStatement::Return { value } => {
                ctx.return_value = if let Some(expr) = value {
                    Some(ctx.evaluator.evaluate(expr, &crate::Tuple::new(vec![]))?)
                } else {
                    None
                };
                ctx.returned = true;
            }

            ProceduralStatement::Execute { sql, into_variables } => {
                let results = (ctx.sql_executor)(sql)?;
                if !into_variables.is_empty() && !results.is_empty() && !results[0].is_empty() {
                    for (i, var_name) in into_variables.iter().enumerate() {
                        if i < results[0].len() {
                            ctx.scope.set(var_name, results[0][i].clone())?;
                        }
                    }
                }
            }

            ProceduralStatement::Raise { level, message, sqlstate, detail, hint } => {
                let msg = if let Some(expr) = message {
                    let val = ctx.evaluator.evaluate(expr, &crate::Tuple::new(vec![]))?;
                    format!("{}", val)
                } else {
                    String::new()
                };

                match level {
                    RaiseLevel::Exception => {
                        let sqlstate_str = sqlstate.as_ref().map(|s| format!(" [SQLSTATE: {}]", s)).unwrap_or_default();
                        return Err(Error::query_execution(format!("RAISE EXCEPTION:{} {}", sqlstate_str, msg)));
                    }
                    RaiseLevel::Warning | RaiseLevel::Notice | RaiseLevel::Info | RaiseLevel::Log | RaiseLevel::Debug => {
                        // Log at appropriate tracing level based on RAISE level
                        match level {
                            RaiseLevel::Warning => tracing::warn!("[RAISE WARNING] {}", msg),
                            RaiseLevel::Notice => tracing::info!("[RAISE NOTICE] {}", msg),
                            RaiseLevel::Info => tracing::info!("[RAISE INFO] {}", msg),
                            RaiseLevel::Log => tracing::debug!("[RAISE LOG] {}", msg),
                            RaiseLevel::Debug => tracing::trace!("[RAISE DEBUG] {}", msg),
                            _ => unreachable!(),
                        }
                    }
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

            ProceduralStatement::Case { when_branches, else_block } => {
                // Searched CASE: evaluates each condition until one is true
                let mut executed = false;
                for (condition, statements) in when_branches {
                    let val = ctx.evaluator.evaluate(condition, &crate::Tuple::new(vec![]))?;
                    if matches!(val, Value::Boolean(true)) {
                        for stmt in statements {
                            Self::execute_statement(stmt, ctx)?;
                            if ctx.returned || ctx.exit_requested {
                                break;
                            }
                        }
                        executed = true;
                        break;
                    }
                }
                if !executed {
                    if let Some(else_stmts) = else_block {
                        for stmt in else_stmts {
                            Self::execute_statement(stmt, ctx)?;
                            if ctx.returned || ctx.exit_requested {
                                break;
                            }
                        }
                    }
                }
            }

            ProceduralStatement::SimpleCase { operand, when_branches, else_block } => {
                // Simple CASE: compares operand against each WHEN value
                let operand_val = ctx.evaluator.evaluate(operand, &crate::Tuple::new(vec![]))?;
                let mut executed = false;
                for (when_val, statements) in when_branches {
                    let val = ctx.evaluator.evaluate(when_val, &crate::Tuple::new(vec![]))?;
                    if operand_val == val {
                        for stmt in statements {
                            Self::execute_statement(stmt, ctx)?;
                            if ctx.returned || ctx.exit_requested {
                                break;
                            }
                        }
                        executed = true;
                        break;
                    }
                }
                if !executed {
                    if let Some(else_stmts) = else_block {
                        for stmt in else_stmts {
                            Self::execute_statement(stmt, ctx)?;
                            if ctx.returned || ctx.exit_requested {
                                break;
                            }
                        }
                    }
                }
            }

            ProceduralStatement::SelectInto { query, variables } => {
                // Execute query and store first row's columns into variables
                let results = (ctx.sql_executor)(query)?;
                if !results.is_empty() && !results[0].is_empty() {
                    for (i, var_name) in variables.iter().enumerate() {
                        if i < results[0].len() {
                            ctx.scope.set(var_name, results[0][i].clone())?;
                        }
                    }
                }
            }

            ProceduralStatement::ExecuteDynamic { sql_expression, into_variables, using_parameters: _ } => {
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
                    "RETURN NEXT requires set-returning function context"
                ));
            }
            ProceduralStatement::ReturnQuery { .. } => {
                return Err(Error::query_execution(
                    "RETURN QUERY requires set-returning function context"
                ));
            }

            // FOR query loops require cursor infrastructure
            ProceduralStatement::ForQuery { record_variable, query, .. } => {
                // Simple implementation: execute query and iterate over results
                let results = (ctx.sql_executor)(query)?;
                for row in results {
                    // Store the row as a composite value in the record variable
                    // For simplicity, store first column value
                    if !row.is_empty() {
                        ctx.scope.set(record_variable, row[0].clone())?;
                    }
                }
            }

            ProceduralStatement::Call { procedure_name, arguments: _ } => {
                return Err(Error::query_execution(format!(
                    "Procedure calls not supported in embedded mode: CALL {}",
                    procedure_name
                )));
            }
        }

        Ok(())
    }

    /// Find a matching exception handler for an error
    ///
    /// Checks each handler's conditions against the error. Conditions are checked in order:
    /// - Named conditions match against PostgreSQL exception names (e.g., "division_by_zero")
    /// - SQLSTATE conditions match against the SQLSTATE code in the error
    /// - OTHERS matches any exception
    #[allow(clippy::indexing_slicing)]
    // SAFETY: String slicing is guarded by `.find()` returning valid byte offsets
    fn find_matching_handler<'a>(
        handlers: &'a [ExceptionHandler],
        error: &Error,
    ) -> Option<&'a ExceptionHandler> {
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
                    ExceptionCondition::SqlState(state) => {
                        sqlstate.as_ref().map(|s| s == state).unwrap_or(false)
                    }
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
    use std::sync::Arc;
    use crate::Schema;

    #[test]
    fn test_variable_scope() {
        let mut scope = VariableScope::new();

        scope.declare(
            "x".to_string(),
            Variable {
                value: Value::Int4(10),
                data_type: Some(DataType::Int4),
                is_constant: false,
                not_null: false,
            },
        ).unwrap();

        assert_eq!(scope.get("x").unwrap().value, Value::Int4(10));

        scope.set("x", Value::Int4(20)).unwrap();
        assert_eq!(scope.get("x").unwrap().value, Value::Int4(20));
    }

    #[test]
    fn test_constant_assignment() {
        let mut scope = VariableScope::new();

        scope.declare(
            "PI".to_string(),
            Variable {
                value: Value::Float8(3.14159),
                data_type: Some(DataType::Float8),
                is_constant: true,
                not_null: false,
            },
        ).unwrap();

        // Should fail
        let result = scope.set("PI", Value::Float8(3.0));
        assert!(result.is_err());
    }

    #[test]
    fn test_nested_scope() {
        let mut parent = VariableScope::new();
        parent.declare(
            "outer".to_string(),
            Variable {
                value: Value::Int4(1),
                data_type: Some(DataType::Int4),
                is_constant: false,
                not_null: false,
            },
        ).unwrap();

        let child = VariableScope::with_parent(parent);

        // Should find variable from parent
        assert!(child.get("outer").is_some());
    }
}
