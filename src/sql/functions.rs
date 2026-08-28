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
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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

/// Registry for user-defined functions and procedures
pub struct FunctionRegistry {
    /// Stored functions
    functions: Arc<RwLock<HashMap<String, StoredFunction>>>,
    /// Stored procedures
    procedures: Arc<RwLock<HashMap<String, StoredProcedure>>>,
}

impl FunctionRegistry {
    /// Create a new function registry
    pub fn new() -> Self {
        Self {
            functions: Arc::new(RwLock::new(HashMap::new())),
            procedures: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a function
    pub fn register_function(&self, func: StoredFunction) -> Result<()> {
        let mut functions = self
            .functions
            .write()
            .map_err(|e| Error::internal(format!("Failed to acquire function lock: {}", e)))?;

        let name = func.name.to_lowercase();

        if functions.contains_key(&name) && !func.or_replace {
            return Err(Error::query_execution(format!(
                "Function '{}' already exists",
                func.name
            )));
        }

        functions.insert(name, func);
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
        procedures.insert(proc.name.to_lowercase(), proc);
        Ok(())
    }

    /// Register a procedure
    pub fn register_procedure(&self, proc: StoredProcedure) -> Result<()> {
        let mut procedures = self
            .procedures
            .write()
            .map_err(|e| Error::internal(format!("Failed to acquire procedure lock: {}", e)))?;

        let name = proc.name.to_lowercase();

        if procedures.contains_key(&name) && !proc.or_replace {
            return Err(Error::query_execution(format!(
                "Procedure '{}' already exists",
                proc.name
            )));
        }

        procedures.insert(name, proc);
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

    /// Drop a function
    pub fn drop_function(&self, name: &str, if_exists: bool) -> Result<bool> {
        let mut functions = self
            .functions
            .write()
            .map_err(|e| Error::internal(format!("Failed to acquire function lock: {}", e)))?;

        let name_lower = name.to_lowercase();

        if functions.remove(&name_lower).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(Error::query_execution(format!("Function '{}' does not exist", name)))
        }
    }

    /// Drop a procedure
    pub fn drop_procedure(&self, name: &str, if_exists: bool) -> Result<bool> {
        let mut procedures = self
            .procedures
            .write()
            .map_err(|e| Error::internal(format!("Failed to acquire procedure lock: {}", e)))?;

        let name_lower = name.to_lowercase();

        if procedures.remove(&name_lower).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(Error::query_execution(format!("Procedure '{}' does not exist", name)))
        }
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
            .get_function(name)
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
    /// cannot evaluate, instead of running it and returning a wrong answer.
    ///
    /// WHY THIS EXISTS (verified at HEAD, and it contradicts the folklore that
    /// "both languages are fully implemented"):
    /// `ProceduralParser::parse_expression` (src/sql/procedural/parser.rs) does
    /// NOT build an expression tree. It captures the RAW SOURCE TEXT of the
    /// expression and returns `LogicalExpr::Literal(Value::String(text))` — its
    /// own comment says "A full implementation would parse this into
    /// LogicalExpr". Every construct whose semantics depend on *evaluating* that
    /// expression therefore misbehaves SILENTLY:
    ///
    /// * `IF <cond> THEN` — `evaluate` yields `Value::String("<cond text>")`,
    ///   which is not `Boolean(true)`, so the condition is ALWAYS false and the
    ///   ELSE branch always runs.
    /// * `WHILE <cond> LOOP` — same: the loop body never runs.
    /// * `x := <expr>` — assigns the literal TEXT of `<expr>` to `x`.
    /// * `DECLARE v INT := <expr>` — same, for the initial value.
    ///
    /// Those are wrong answers, not errors, and this repo has shipped that class
    /// of bug before. So the FUNCTION path (which is what UDF invocation newly
    /// reaches) accepts only the constructs that are genuinely implemented —
    /// SQL statements, `SELECT … INTO`, `RETURN`, `NULL` and nested blocks, all
    /// of which route through the `$`-sigil interpolator rather than the
    /// expression parser — and names anything else in a loud error.
    ///
    /// PROCEDURES ARE DELIBERATELY NOT GATED. `execute_plpgsql_procedure` ships
    /// today and this gate is not applied to it: adding it there would newly
    /// break bodies that users already run. The underlying parser defect is
    /// filed (ROADMAP_V5 §2.9) and is the real fix; this gate only stops the
    /// NEW path from manufacturing wrong answers.
    fn reject_unevaluable_constructs(func_name: &str, block: &ProceduralBlock) -> Result<()> {
        let refuse = |construct: &str| -> Error {
            Error::query_execution(format!(
                "Function '{}' was NOT executed: its PL/pgSQL body uses {}, which HeliosDB Nano \
                 cannot evaluate — the procedural expression parser captures expression text \
                 instead of parsing it, so the construct would silently produce a wrong result. \
                 Supported in a PL/pgSQL function body today: SQL statements, `SELECT … INTO <var>`, \
                 `RETURN <expr>` and nested BEGIN/END blocks, with parameters and variables \
                 referenced through the mandatory `$` sigil (`$name` / `$1`). Rewrite the function \
                 with LANGUAGE sql, or express the logic in SQL (e.g. CASE inside the RETURN \
                 expression).",
                func_name, construct
            ))
        };

        for decl in &block.declarations {
            if decl.default.is_some() {
                return Err(refuse(&format!("a DECLARE default value (`{} … := …`)", decl.name)));
            }
        }

        Self::reject_unevaluable_statements(&refuse, &block.statements)?;

        if !block.exception_handlers.is_empty() {
            return Err(refuse("an EXCEPTION handler"));
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
                // These three are the interpolator-backed paths: the body SQL is
                // rendered through `crate::sql::interpolate` and handed to the
                // executor verbatim, so no expression evaluation is involved.
                ProceduralStatement::Execute { .. }
                | ProceduralStatement::SelectInto { .. }
                | ProceduralStatement::Null => {}
                // `RETURN` is evaluated as SQL — see
                // `ProceduralExecutor::evaluate_return_value`.
                ProceduralStatement::Return { .. } => {}
                ProceduralStatement::Block(inner) => {
                    for decl in &inner.declarations {
                        if decl.default.is_some() {
                            return Err(refuse(&format!("a DECLARE default value (`{} … := …`)", decl.name)));
                        }
                    }
                    Self::reject_unevaluable_statements(refuse, &inner.statements)?;
                    if !inner.exception_handlers.is_empty() {
                        return Err(refuse("an EXCEPTION handler"));
                    }
                }
                ProceduralStatement::Assignment { .. } => return Err(refuse("a `:=` assignment")),
                ProceduralStatement::If { .. } => return Err(refuse("an IF statement")),
                ProceduralStatement::Case { .. } | ProceduralStatement::SimpleCase { .. } => {
                    return Err(refuse("a CASE statement"))
                }
                ProceduralStatement::Loop { .. } => return Err(refuse("a LOOP")),
                ProceduralStatement::While { .. } => return Err(refuse("a WHILE loop")),
                ProceduralStatement::ForNumeric { .. } | ProceduralStatement::ForQuery { .. } => {
                    return Err(refuse("a FOR loop"))
                }
                ProceduralStatement::Exit { .. } => return Err(refuse("an EXIT statement")),
                ProceduralStatement::Continue { .. } => return Err(refuse("a CONTINUE statement")),
                ProceduralStatement::ReturnNext { .. } | ProceduralStatement::ReturnQuery { .. } => {
                    return Err(refuse("RETURN NEXT / RETURN QUERY (set-returning functions)"))
                }
                ProceduralStatement::Raise { .. } => return Err(refuse("a RAISE statement")),
                ProceduralStatement::ExecuteDynamic { .. } => return Err(refuse("EXECUTE of dynamic SQL")),
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
        ProceduralExecutor::execute_block(&block, &mut ctx)?;

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
            .get_procedure(name)
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

        ProceduralExecutor::execute_block(&block, &mut ctx)?;
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
