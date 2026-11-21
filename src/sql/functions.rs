//! Function Registry and Execution
//!
//! This module provides storage and execution of user-defined functions and procedures.
//! Functions are stored in-memory and can be called from SQL queries.

use super::evaluator::Evaluator;
use super::interpolate::{interpolate_sql_placeholders, value_to_sql_literal, Placeholder};
use super::logical_plan::{FunctionParam, ParamMode};
use super::procedural::{ExecutionContext, ProceduralBlock, ProceduralExecutor, ProceduralParser, ProceduralStatement};
use crate::{DataType, Error, Result, Value};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock, RwLockWriteGuard};

/// Stored function definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFunction {
    /// Function name
    pub name: String,
    /// Whether this can replace existing
    pub or_replace: bool,
    /// Function parameters
    pub params: Vec<FunctionParam>,
    /// Return type
    pub return_type: Option<DataType>,
    /// Function body (raw source)
    pub body: String,
    /// Language (plpgsql, sql)
    pub language: String,
    /// Volatility (IMMUTABLE, STABLE, VOLATILE)
    pub volatility: Option<String>,
    /// Creation timestamp
    pub created_at: u64,
}

/// Stored procedure definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredProcedure {
    /// Procedure name
    pub name: String,
    /// Whether this can replace existing
    pub or_replace: bool,
    /// Procedure parameters
    pub params: Vec<FunctionParam>,
    /// Procedure body (raw source)
    pub body: String,
    /// Language (plpgsql, sql)
    pub language: String,
    /// Creation timestamp
    pub created_at: u64,
}

/// A cold DDL edit owns the existing map write guard until its durable
/// outcome is settled. Readers never observe an uncommitted routine body.
/// Previous entries move into this guard instead of cloning their bodies.
pub(crate) struct RoutineDdlEdit<'a, T> {
    entries: RwLockWriteGuard<'a, HashMap<String, T>>,
    unavailable: &'a RwLock<HashMap<&'static str, HashSet<String>>>,
    kind: &'static str,
    name: String,
    previous: Option<T>,
    outcome: RoutineEditOutcome,
}

#[derive(Clone, Copy)]
enum RoutineEditOutcome {
    Unwritten,
    Applied,
    Unknown,
}

impl<T: Serialize> RoutineDdlEdit<'_, T> {
    pub(crate) fn unchanged(&self) -> Result<bool> {
        match (self.previous.as_ref(), self.entries.get(&self.name)) {
            (None, None) => Ok(true),
            (Some(before), Some(after)) => {
                let before = bincode::serialize(before)
                    .map_err(|error| Error::storage(format!("Cannot compare routine BEFORE: {error}")))?;
                let after = bincode::serialize(after)
                    .map_err(|error| Error::storage(format!("Cannot compare routine AFTER: {error}")))?;
                Ok(before == after)
            }
            _ => Ok(false),
        }
    }
}

impl<T> RoutineDdlEdit<'_, T> {
    pub(crate) fn begin_commit(&mut self) {
        // A panic after entering the commit must not blindly restore an old
        // body over a possibly-applied group.
        self.outcome = RoutineEditOutcome::Unknown;
    }

    pub(crate) fn settle(&mut self, applied: Option<bool>) {
        self.outcome = match applied {
            Some(true) => RoutineEditOutcome::Applied,
            Some(false) => RoutineEditOutcome::Unwritten,
            None => RoutineEditOutcome::Unknown,
        };
    }
}

impl<T> Drop for RoutineDdlEdit<'_, T> {
    fn drop(&mut self) {
        match self.outcome {
            RoutineEditOutcome::Applied => {}
            RoutineEditOutcome::Unwritten => match self.previous.take() {
                Some(previous) => {
                    self.entries.insert(self.name.clone(), previous);
                }
                None => {
                    self.entries.remove(&self.name);
                }
            },
            RoutineEditOutcome::Unknown => {
                // No durable repair: retire only this unreconstructible name.
                // Every mutator checks the fence under this same map owner.
                if let Ok(mut unavailable) = self.unavailable.write() {
                    unavailable.entry(self.kind).or_default().insert(self.name.clone());
                }
                self.entries.remove(&self.name);
            }
        }
    }
}

/// Registry for user-defined functions and procedures
pub struct FunctionRegistry {
    /// Stored functions
    functions: Arc<RwLock<HashMap<String, StoredFunction>>>,
    /// Stored procedures
    procedures: Arc<RwLock<HashMap<String, StoredProcedure>>>,
    /// Only unresolved native outcomes populate this cold writer fence.
    /// Read/query paths keep their original map lookup and allocation cost.
    unavailable: RwLock<HashMap<&'static str, HashSet<String>>>,
    /// `[procedural] max_loop_iterations` (0 = unlimited).
    max_loop_iterations: std::sync::atomic::AtomicU64,
    /// `[storage] statement_timeout_ms` for routine loops (0 = none).
    default_statement_timeout_ms: std::sync::atomic::AtomicU64,
}

impl FunctionRegistry {
    /// Create a new function registry
    pub fn new() -> Self {
        Self {
            functions: Arc::new(RwLock::new(HashMap::new())),
            procedures: Arc::new(RwLock::new(HashMap::new())),
            unavailable: RwLock::new(HashMap::new()),
            max_loop_iterations: std::sync::atomic::AtomicU64::new(0),
            default_statement_timeout_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Install the loop limits a PL/pgSQL routine runs under: `[procedural]
    /// max_loop_iterations` (0 = unlimited) and the configured statement
    /// timeout, which every loop iteration checks.
    pub fn set_procedural_limits(&self, max_loop_iterations: u64, statement_timeout_ms: Option<u64>) {
        use std::sync::atomic::Ordering;
        self.max_loop_iterations.store(max_loop_iterations, Ordering::Relaxed);
        self.default_statement_timeout_ms
            .store(statement_timeout_ms.unwrap_or(0), Ordering::Relaxed);
    }

    fn apply_procedural_limits(&self, ctx: &mut ExecutionContext<'_>) {
        use std::sync::atomic::Ordering;
        ctx.max_loop_iterations = self.max_loop_iterations.load(Ordering::Relaxed);
        let timeout = self.default_statement_timeout_ms.load(Ordering::Relaxed);
        ctx.default_statement_timeout_ms = (timeout > 0).then_some(timeout);
    }

    fn check_routine_available(&self, kind: &'static str, name: &str) -> Result<()> {
        let unavailable = self
            .unavailable
            .read()
            .map_err(|error| Error::storage(format!("Cannot read routine availability: {error}")))?;
        if unavailable.get(kind).is_some_and(|names| names.contains(name)) {
            return Err(Error::storage(format!("{kind} '{name}' is unavailable after an unresolved native DDL outcome; reopen to reload its durable definition")));
        }
        Ok(())
    }

    pub(crate) fn begin_function_create(&self, func: StoredFunction) -> Result<RoutineDdlEdit<'_, StoredFunction>> {
        let mut entries = self
            .functions
            .write()
            .map_err(|error| Error::internal(format!("Failed to acquire function lock: {error}")))?;
        let name = func.name.to_lowercase();
        self.check_routine_available("function", &name)?;
        if entries.contains_key(&name) && !func.or_replace {
            return Err(Error::query_execution(format!(
                "Function '{}' already exists",
                func.name
            )));
        }
        let previous = entries.insert(name.clone(), func);
        Ok(RoutineDdlEdit {
            entries,
            unavailable: &self.unavailable,
            kind: "function",
            name,
            previous,
            outcome: RoutineEditOutcome::Unwritten,
        })
    }

    /// Register a function without a durable DDL owner (standalone registry API).
    pub fn register_function(&self, func: StoredFunction) -> Result<()> {
        let mut edit = self.begin_function_create(func)?;
        edit.settle(Some(true));
        Ok(())
    }

    /// Load a function definition read back from durable storage.
    ///
    /// Deliberately NOT `register_function`: loading is not user DDL. The
    /// open-time loader replays whatever `meta:function:<name>` records survived
    /// the last shutdown, and every one of them was already accepted once — so
    /// the `or_replace` "already exists" gate must not fire (it would make the
    /// SECOND record for a name fail on a re-open, or make a reload after a
    /// partial load error out). Same key derivation as `register_function`:
    /// lowercase name, no signature (no overloading — see the module docs).
    pub fn load_function(&self, func: StoredFunction) -> Result<()> {
        let mut functions = self
            .functions
            .write()
            .map_err(|e| Error::internal(format!("Failed to acquire function lock: {}", e)))?;
        self.check_routine_available("function", &func.name.to_lowercase())?;
        functions.insert(func.name.to_lowercase(), func);
        Ok(())
    }

    /// Load a procedure definition read back from durable storage. See
    /// [`FunctionRegistry::load_function`].
    pub fn load_procedure(&self, proc: StoredProcedure) -> Result<()> {
        let mut procedures = self
            .procedures
            .write()
            .map_err(|e| Error::internal(format!("Failed to acquire procedure lock: {}", e)))?;
        self.check_routine_available("procedure", &proc.name.to_lowercase())?;
        procedures.insert(proc.name.to_lowercase(), proc);
        Ok(())
    }

    pub(crate) fn begin_procedure_create(&self, proc: StoredProcedure) -> Result<RoutineDdlEdit<'_, StoredProcedure>> {
        let mut entries = self
            .procedures
            .write()
            .map_err(|error| Error::internal(format!("Failed to acquire procedure lock: {error}")))?;
        let name = proc.name.to_lowercase();
        self.check_routine_available("procedure", &name)?;
        if entries.contains_key(&name) && !proc.or_replace {
            return Err(Error::query_execution(format!(
                "Procedure '{}' already exists",
                proc.name
            )));
        }
        let previous = entries.insert(name.clone(), proc);
        Ok(RoutineDdlEdit {
            entries,
            unavailable: &self.unavailable,
            kind: "procedure",
            name,
            previous,
            outcome: RoutineEditOutcome::Unwritten,
        })
    }

    /// Register a procedure without a durable DDL owner.
    pub fn register_procedure(&self, proc: StoredProcedure) -> Result<()> {
        let mut edit = self.begin_procedure_create(proc)?;
        edit.settle(Some(true));
        Ok(())
    }

    /// Get a function by name
    pub fn get_function(&self, name: &str) -> Option<StoredFunction> {
        let functions = self.functions.read().ok()?;
        functions.get(&name.to_lowercase()).cloned()
    }

    /// Get a procedure by name
    pub fn get_procedure(&self, name: &str) -> Option<StoredProcedure> {
        let procedures = self.procedures.read().ok()?;
        procedures.get(&name.to_lowercase()).cloned()
    }

    pub(crate) fn begin_function_drop(
        &self,
        name: &str,
        if_exists: bool,
    ) -> Result<RoutineDdlEdit<'_, StoredFunction>> {
        let mut entries = self
            .functions
            .write()
            .map_err(|error| Error::internal(format!("Failed to acquire function lock: {error}")))?;
        let normalized = name.to_lowercase();
        self.check_routine_available("function", &normalized)?;
        if !entries.contains_key(&normalized) && !if_exists {
            return Err(Error::query_execution(format!("Function '{name}' does not exist")));
        }
        let previous = entries.remove(&normalized);
        Ok(RoutineDdlEdit {
            entries,
            unavailable: &self.unavailable,
            kind: "function",
            name: normalized,
            previous,
            outcome: RoutineEditOutcome::Unwritten,
        })
    }

    pub(crate) fn begin_procedure_drop(
        &self,
        name: &str,
        if_exists: bool,
    ) -> Result<RoutineDdlEdit<'_, StoredProcedure>> {
        let mut entries = self
            .procedures
            .write()
            .map_err(|error| Error::internal(format!("Failed to acquire procedure lock: {error}")))?;
        let normalized = name.to_lowercase();
        self.check_routine_available("procedure", &normalized)?;
        if !entries.contains_key(&normalized) && !if_exists {
            return Err(Error::query_execution(format!("Procedure '{name}' does not exist")));
        }
        let previous = entries.remove(&normalized);
        Ok(RoutineDdlEdit {
            entries,
            unavailable: &self.unavailable,
            kind: "procedure",
            name: normalized,
            previous,
            outcome: RoutineEditOutcome::Unwritten,
        })
    }

    // Preserve the public Option lookup and its healthy map/body-clone cost.
    // Only a negative invocation lookup consults the cold unresolved-name fence.
    fn get_function_checked(&self, name: &str) -> Result<Option<StoredFunction>> {
        let function = self.get_function(name);
        if function.is_none() {
            self.check_routine_available("function", &name.to_lowercase())?;
        }
        Ok(function)
    }

    fn get_procedure_checked(&self, name: &str) -> Result<Option<StoredProcedure>> {
        let procedure = self.get_procedure(name);
        if procedure.is_none() {
            self.check_routine_available("procedure", &name.to_lowercase())?;
        }
        Ok(procedure)
    }

    pub(crate) fn function_exists_checked(&self, name: &str) -> Result<bool> {
        let exists = self.function_exists(name);
        if !exists {
            self.check_routine_available("function", &name.to_lowercase())?;
        }
        Ok(exists)
    }

    /// Drop a function without a durable DDL owner.
    pub fn drop_function(&self, name: &str, if_exists: bool) -> Result<bool> {
        let mut edit = self.begin_function_drop(name, if_exists)?;
        let existed = edit.previous.is_some();
        edit.settle(Some(true));
        Ok(existed)
    }

    /// Drop a procedure without a durable DDL owner.
    pub fn drop_procedure(&self, name: &str, if_exists: bool) -> Result<bool> {
        let mut edit = self.begin_procedure_drop(name, if_exists)?;
        let existed = edit.previous.is_some();
        edit.settle(Some(true));
        Ok(existed)
    }

    /// Check if a function exists
    pub fn function_exists(&self, name: &str) -> bool {
        self.functions
            .read()
            .map(|f| f.contains_key(&name.to_lowercase()))
            .unwrap_or(false)
    }

    /// Check if a procedure exists
    pub fn procedure_exists(&self, name: &str) -> bool {
        self.procedures
            .read()
            .map(|p| p.contains_key(&name.to_lowercase()))
            .unwrap_or(false)
    }

    /// List all function names
    pub fn list_functions(&self) -> Vec<String> {
        self.functions
            .read()
            .map(|f| f.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// List all procedure names
    pub fn list_procedures(&self) -> Vec<String> {
        self.procedures
            .read()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Execute a stored function with arguments
    pub fn execute_function(
        &self,
        name: &str,
        args: &[Value],
        sql_executor: impl FnMut(&str) -> Result<Vec<Vec<Value>>>,
    ) -> Result<Value> {
        let func = self
            .get_function_checked(name)?
            .ok_or_else(|| Error::query_execution(format!("Function '{}' does not exist", name)))?;

        // Validate argument count
        let required_params: Vec<_> = func
            .params
            .iter()
            .filter(|p| p.default.is_none() && p.mode != ParamMode::Out)
            .collect();

        if args.len() < required_params.len() {
            return Err(Error::query_execution(format!(
                "Function '{}' requires at least {} arguments, got {}",
                name,
                required_params.len(),
                args.len()
            )));
        }

        let max_in_params = func.params.iter().filter(|p| p.mode != ParamMode::Out).count();

        if args.len() > max_in_params {
            return Err(Error::query_execution(format!(
                "Function '{}' accepts at most {} arguments, got {}",
                name,
                max_in_params,
                args.len()
            )));
        }

        // Execute based on language
        match func.language.to_lowercase().as_str() {
            "sql" => self.execute_sql_function(&func, args, sql_executor),
            "plpgsql" => self.execute_plpgsql_function(&func, args, sql_executor),
            lang => Err(Error::query_execution(format!(
                "Unsupported function language: {}",
                lang
            ))),
        }
    }

    /// Execute a SQL language function
    // SAFETY: results[0][0] guarded by is_empty() checks.
    #[allow(clippy::indexing_slicing)]
    fn execute_sql_function(
        &self,
        func: &StoredFunction,
        args: &[Value],
        mut sql_executor: impl FnMut(&str) -> Result<Vec<Vec<Value>>>,
    ) -> Result<Value> {
        // For SQL functions the body is raw SQL: interpolate `$1`/`$<paramname>` from the
        // call's arguments. See `crate::sql::interpolate` — the shared scanner is the only
        // implementation, and it never substitutes inside literals or comments.
        let body = interpolate_sql_placeholders(&func.body, |ph| resolve_from_args(&func.params, args, ph));

        // Execute the SQL and get the result
        let results = sql_executor(&body)?;

        if results.is_empty() || results[0].is_empty() {
            Ok(Value::Null)
        } else {
            Ok(results[0][0].clone())
        }
    }

    /// Refuse a PL/pgSQL FUNCTION body that contains a construct this engine
    /// cannot run in a function, instead of running it and returning a wrong
    /// answer.
    ///
    /// Control flow, assignments and DECLARE defaults are evaluated (see
    /// `ProceduralExecutor::eval_expr`: each procedural expression runs as
    /// `SELECT <text>` after `$`-sigil interpolation), so they are admitted.
    /// Still refused: RAISE, EXECUTE of dynamic SQL, cursors, RETURN NEXT /
    /// RETURN QUERY, CALL, PRINT and EXCEPTION handlers. Procedures are not
    /// gated.
    fn reject_unevaluable_constructs(func_name: &str, block: &ProceduralBlock) -> Result<()> {
        let refuse = |construct: &str| -> Error {
            Error::query_execution(format!(
                "Function '{}' was NOT executed: its PL/pgSQL body uses {}, which HeliosDB Nano \
                 does not run inside a function. Supported in a PL/pgSQL function body: SQL \
                 statements, `SELECT … INTO <var>`, PERFORM, `:=` assignments, DECLARE defaults, \
                 IF/ELSIF/ELSE, CASE, LOOP, WHILE, FOR (numeric and over a query), EXIT, \
                 CONTINUE, RETURN and nested BEGIN/END blocks, with parameters and variables \
                 referenced through the mandatory `$` sigil (`$name` / `$1`).",
                func_name, construct
            ))
        };
        Self::reject_unevaluable_block(&refuse, block)
    }

    fn reject_unevaluable_block(refuse: &dyn Fn(&str) -> Error, block: &ProceduralBlock) -> Result<()> {
        Self::reject_unevaluable_statements(refuse, &block.statements)?;
        for handler in &block.exception_handlers {
            Self::reject_unevaluable_statements(refuse, &handler.body)?;
        }
        Ok(())
    }

    /// Statement-level half of [`FunctionRegistry::reject_unevaluable_constructs`].
    ///
    /// Written as an EXHAUSTIVE allow-list on purpose: there is NO catch-all
    /// `_` arm, so a `ProceduralStatement` variant added later is a compile
    /// error here rather than being silently admitted as evaluable. Admitting a
    /// construct that is not really implemented is exactly the failure mode this
    /// gate exists to prevent. Do not add a `_ =>` arm.
    fn reject_unevaluable_statements(refuse: &dyn Fn(&str) -> Error, statements: &[ProceduralStatement]) -> Result<()> {
        for stmt in statements {
            match stmt {
                ProceduralStatement::Execute { .. }
                | ProceduralStatement::SelectInto { .. }
                | ProceduralStatement::Raise { .. }
                | ProceduralStatement::ExecuteDynamic { .. }
                | ProceduralStatement::Null
                | ProceduralStatement::Return { .. }
                | ProceduralStatement::Assignment { .. }
                | ProceduralStatement::Exit { .. }
                | ProceduralStatement::Continue { .. } => {}
                ProceduralStatement::Block(inner) => Self::reject_unevaluable_block(refuse, inner)?,
                ProceduralStatement::If {
                    then_block,
                    elsif_branches,
                    else_block,
                    ..
                } => {
                    Self::reject_unevaluable_statements(refuse, then_block)?;
                    for (_, branch) in elsif_branches {
                        Self::reject_unevaluable_statements(refuse, branch)?;
                    }
                    if let Some(branch) = else_block {
                        Self::reject_unevaluable_statements(refuse, branch)?;
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
                    for (_, branch) in when_branches {
                        Self::reject_unevaluable_statements(refuse, branch)?;
                    }
                    if let Some(branch) = else_block {
                        Self::reject_unevaluable_statements(refuse, branch)?;
                    }
                }
                ProceduralStatement::Loop { body, .. }
                | ProceduralStatement::While { body, .. }
                | ProceduralStatement::ForNumeric { body, .. }
                | ProceduralStatement::ForQuery { body, .. } => Self::reject_unevaluable_statements(refuse, body)?,
                ProceduralStatement::ReturnNext { .. } | ProceduralStatement::ReturnQuery { .. } => {
                    return Err(refuse("RETURN NEXT / RETURN QUERY (set-returning functions)"))
                }
                ProceduralStatement::Print { .. } => return Err(refuse("a PRINT statement")),
                ProceduralStatement::OpenCursor { .. }
                | ProceduralStatement::FetchCursor { .. }
                | ProceduralStatement::CloseCursor { .. } => return Err(refuse("a cursor operation")),
                ProceduralStatement::Call { .. } => return Err(refuse("a CALL statement")),
            }
        }
        Ok(())
    }

    /// Execute a PL/pgSQL function
    // SAFETY: args[i] guarded by i < args.len() check.
    #[allow(clippy::indexing_slicing)]
    fn execute_plpgsql_function(
        &self,
        func: &StoredFunction,
        args: &[Value],
        sql_executor: impl FnMut(&str) -> Result<Vec<Vec<Value>>>,
    ) -> Result<Value> {
        // Parse the function body into a procedural block
        let mut parser = ProceduralParser::new(&func.body);
        let block = parser
            .parse_block()
            .map_err(|e| Error::query_execution(format!("Failed to parse function body: {}", e)))?;

        // LOUD GATE — see `reject_unevaluable_constructs`. A PL/pgSQL body whose
        // control flow or assignments depend on the procedural EXPRESSION parser
        // cannot be executed correctly today, and running it anyway silently
        // produces a wrong answer. Refuse instead.
        Self::reject_unevaluable_constructs(&func.name, &block)?;

        // Create execution context
        let schema = Arc::new(crate::Schema { columns: vec![] });
        let evaluator = Evaluator::new(schema);
        let mut ctx = ExecutionContext::new(&evaluator, sql_executor);
        self.apply_procedural_limits(&mut ctx);
        ctx.set_routine_name(&func.name);

        // Bind parameters to context
        for (i, param) in func.params.iter().enumerate() {
            if param.mode == ParamMode::Out {
                continue;
            }

            let value = if i < args.len() {
                args[i].clone()
            } else if let Some(ref default) = param.default {
                evaluator.evaluate(default, &crate::Tuple::new(vec![]))?
            } else {
                Value::Null
            };

            ctx.scope.declare(
                param.name.clone(),
                super::procedural::Variable {
                    value,
                    data_type: Some(param.data_type.clone()),
                    is_constant: false,
                    not_null: false,
                },
            )?;
        }

        // `$1..$N` in body SQL resolve against the call's arguments, exactly as they do
        // in a `LANGUAGE sql` body (PostgreSQL allows `$n` in PL/pgSQL too).
        ctx.positional_params = args.to_vec();

        // Execute the block
        ProceduralExecutor::execute_routine(&block, &mut ctx)?;

        // Return the result
        Ok(ctx.return_value.unwrap_or(Value::Null))
    }

    /// Execute a stored procedure with arguments
    pub fn execute_procedure(
        &self,
        name: &str,
        args: &[Value],
        sql_executor: impl FnMut(&str) -> Result<Vec<Vec<Value>>>,
    ) -> Result<()> {
        let proc = self
            .get_procedure_checked(name)?
            .ok_or_else(|| Error::query_execution(format!("Procedure '{}' does not exist", name)))?;

        // Execute based on language
        match proc.language.to_lowercase().as_str() {
            "sql" => self.execute_sql_procedure(&proc, args, sql_executor),
            "plpgsql" => self.execute_plpgsql_procedure(&proc, args, sql_executor),
            lang => Err(Error::query_execution(format!(
                "Unsupported procedure language: {}",
                lang
            ))),
        }
    }

    /// Execute a SQL language procedure
    fn execute_sql_procedure(
        &self,
        proc: &StoredProcedure,
        args: &[Value],
        mut sql_executor: impl FnMut(&str) -> Result<Vec<Vec<Value>>>,
    ) -> Result<()> {
        // Same interpolation contract as `execute_sql_function`; see
        // `crate::sql::interpolate`.
        let body = interpolate_sql_placeholders(&proc.body, |ph| resolve_from_args(&proc.params, args, ph));

        sql_executor(&body)?;
        Ok(())
    }

    /// Execute a PL/pgSQL procedure
    // SAFETY: args[i] guarded by i < args.len() check.
    #[allow(clippy::indexing_slicing)]
    fn execute_plpgsql_procedure(
        &self,
        proc: &StoredProcedure,
        args: &[Value],
        sql_executor: impl FnMut(&str) -> Result<Vec<Vec<Value>>>,
    ) -> Result<()> {
        let mut parser = ProceduralParser::new(&proc.body);
        let block = parser
            .parse_block()
            .map_err(|e| Error::query_execution(format!("Failed to parse procedure body: {}", e)))?;

        let schema = Arc::new(crate::Schema { columns: vec![] });
        let evaluator = Evaluator::new(schema);
        let mut ctx = ExecutionContext::new(&evaluator, sql_executor);
        self.apply_procedural_limits(&mut ctx);
        ctx.set_routine_name(&proc.name);

        for (i, param) in proc.params.iter().enumerate() {
            if param.mode == ParamMode::Out {
                continue;
            }

            let value = if i < args.len() {
                args[i].clone()
            } else if let Some(ref default) = param.default {
                evaluator.evaluate(default, &crate::Tuple::new(vec![]))?
            } else {
                Value::Null
            };

            ctx.scope.declare(
                param.name.clone(),
                super::procedural::Variable {
                    value,
                    data_type: Some(param.data_type.clone()),
                    is_constant: false,
                    not_null: false,
                },
            )?;
        }

        // Same as the function variant: make `$1..$N` resolvable inside the body.
        ctx.positional_params = args.to_vec();

        ProceduralExecutor::execute_routine(&block, &mut ctx)?;
        Ok(())
    }
}

impl Default for FunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve one placeholder from a routine's declared parameters and the call's arguments.
///
/// Shared by the `LANGUAGE sql` function and procedure paths. Behavioural contract,
/// unchanged from the `String::replace` implementation this replaced:
///
/// * positional indices range over the ARGUMENTS, not the declared parameters, so a
///   caller who passes more arguments than the routine declares can still reference the
///   extras as `$N`;
/// * a named reference resolves only when an argument was actually supplied for that
///   parameter — a reference to a defaulted-but-unsupplied parameter stays verbatim and
///   errors downstream, because this path has never evaluated parameter defaults;
/// * name matching is exact-case (PostgreSQL folds unquoted identifiers; Nano does not);
/// * a body that never mentions a parameter silently discards the argument.
fn resolve_from_args(params: &[FunctionParam], args: &[Value], ph: Placeholder<'_>) -> Option<String> {
    match ph {
        // `checked_sub` — not `args.get(n - 1)`, which would underflow-panic on `$0`.
        Placeholder::Positional(n) => n.checked_sub(1).and_then(|i| args.get(i)).map(value_to_sql_literal),
        Placeholder::Named(name) => params
            .iter()
            .position(|p| p.name == name)
            .and_then(|i| args.get(i))
            .map(value_to_sql_literal),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_register_function() {
        let registry = FunctionRegistry::new();

        let func = StoredFunction {
            name: "add_numbers".to_string(),
            or_replace: false,
            params: vec![
                FunctionParam {
                    name: "a".to_string(),
                    data_type: DataType::Int4,
                    mode: ParamMode::In,
                    default: None,
                },
                FunctionParam {
                    name: "b".to_string(),
                    data_type: DataType::Int4,
                    mode: ParamMode::In,
                    default: None,
                },
            ],
            return_type: Some(DataType::Int4),
            body: "SELECT $1 + $2".to_string(),
            language: "sql".to_string(),
            volatility: Some("IMMUTABLE".to_string()),
            created_at: 0,
        };

        registry.register_function(func).unwrap();
        assert!(registry.function_exists("add_numbers"));
        assert!(registry.function_exists("ADD_NUMBERS")); // case insensitive
    }

    #[test]
    fn test_duplicate_function_error() {
        let registry = FunctionRegistry::new();

        let func = StoredFunction {
            name: "my_func".to_string(),
            or_replace: false,
            params: vec![],
            return_type: Some(DataType::Int4),
            body: "SELECT 1".to_string(),
            language: "sql".to_string(),
            volatility: None,
            created_at: 0,
        };

        registry.register_function(func.clone()).unwrap();

        // Second registration should fail
        let result = registry.register_function(func);
        assert!(result.is_err());
    }

    #[test]
    fn test_or_replace() {
        let registry = FunctionRegistry::new();

        let func1 = StoredFunction {
            name: "my_func".to_string(),
            or_replace: false,
            params: vec![],
            return_type: Some(DataType::Int4),
            body: "SELECT 1".to_string(),
            language: "sql".to_string(),
            volatility: None,
            created_at: 0,
        };

        registry.register_function(func1).unwrap();

        let func2 = StoredFunction {
            name: "my_func".to_string(),
            or_replace: true,
            params: vec![],
            return_type: Some(DataType::Int4),
            body: "SELECT 2".to_string(),
            language: "sql".to_string(),
            volatility: None,
            created_at: 0,
        };

        // Should succeed with or_replace
        registry.register_function(func2).unwrap();

        let stored = registry.get_function("my_func").unwrap();
        assert_eq!(stored.body, "SELECT 2");
    }

    #[test]
    fn test_drop_function() {
        let registry = FunctionRegistry::new();

        let func = StoredFunction {
            name: "to_drop".to_string(),
            or_replace: false,
            params: vec![],
            return_type: Some(DataType::Int4),
            body: "SELECT 1".to_string(),
            language: "sql".to_string(),
            volatility: None,
            created_at: 0,
        };

        registry.register_function(func).unwrap();
        assert!(registry.function_exists("to_drop"));

        registry.drop_function("to_drop", false).unwrap();
        assert!(!registry.function_exists("to_drop"));
    }

    #[test]
    fn test_execute_sql_function() {
        let registry = FunctionRegistry::new();

        let func = StoredFunction {
            name: "double_it".to_string(),
            or_replace: false,
            params: vec![FunctionParam {
                name: "x".to_string(),
                data_type: DataType::Int4,
                mode: ParamMode::In,
                default: None,
            }],
            return_type: Some(DataType::Int4),
            body: "SELECT $1 * 2".to_string(),
            language: "sql".to_string(),
            volatility: Some("IMMUTABLE".to_string()),
            created_at: 0,
        };

        registry.register_function(func).unwrap();

        // Mock SQL executor
        let result = registry
            .execute_function("double_it", &[Value::Int4(21)], |sql| {
                // The SQL should be "SELECT 21 * 2"
                assert!(sql.contains("21"));
                Ok(vec![vec![Value::Int4(42)]])
            })
            .unwrap();

        assert_eq!(result, Value::Int4(42));
    }
}

#[cfg(all(test, feature = "internal-tests"))]
#[path = "routine_outcome_guard_tests.rs"]
mod routine_outcome_guard_tests;

#[cfg(all(test, feature = "internal-tests"))]
#[path = "routine_lookup_fence_tests.rs"]
mod routine_lookup_fence_tests;

#[cfg(all(test, feature = "internal-tests"))]
#[path = "routine_bridge_owner_tests.rs"]
mod routine_bridge_owner_tests;

#[cfg(all(test, feature = "internal-tests"))]
#[path = "routine_trigger_lookup_tests.rs"]
mod routine_trigger_lookup_tests;
