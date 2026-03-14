//! Volcano-model query executor
//!
//! This module implements a simple iterator-based query execution engine
//! using the Volcano model (also known as the iterator model or pipeline model).
//!
//! Each operator implements a simple interface:
//! - `next()` - returns the next tuple or None when exhausted
//!
//! Operators are composed into a tree that processes data one tuple at a time.

use crate::{Result, Error, Tuple, Schema};
use crate::sql::LogicalPlan;
use crate::storage::StorageEngine;
use std::sync::Arc;
use std::time::{Duration, Instant};

// Re-export submodules
pub mod scan;
pub mod filter;
pub mod project;
pub mod join;
pub mod aggregate;
pub mod ddl;
pub mod phase3;
pub mod explain;
pub mod window;
pub mod set_ops;

// Re-export operators for public API
pub use scan::{ScanOperator, VectorScanOperator, MaterializedOperator, GenerateSeriesOperator, UnnestOperator};
pub use filter::FilterOperator;
pub use project::{ProjectOperator, LimitOperator};
pub use join::{NestedLoopJoinOperator, HashJoinOperator};
pub use aggregate::{AggregateOperator, SortOperator};
pub use window::WindowOperator;
pub use set_ops::{UnionOperator, IntersectOperator, ExceptOperator};

/// Create a schema for COUNT(*) fast path results (single Int8 column).
fn count_star_schema() -> Arc<Schema> {
    Arc::new(Schema {
        columns: vec![crate::Column {
            name: "agg_0".to_string(),
            data_type: crate::DataType::Int8,
            nullable: false,
            primary_key: false,
            source_table: None,
            source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
        }],
    })
}

/// DualScan operator for SELECT without FROM
///
/// Returns a single row with no columns, used as input for
/// expression evaluation in queries like `SELECT 1+1`.
pub struct DualScanOperator {
    /// Whether we've returned the single row yet
    exhausted: bool,
}

impl DualScanOperator {
    /// Create a new DualScan operator
    pub fn new() -> Self {
        Self { exhausted: false }
    }
}

impl Default for DualScanOperator {
    fn default() -> Self {
        Self::new()
    }
}

impl PhysicalOperator for DualScanOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.exhausted {
            Ok(None)
        } else {
            self.exhausted = true;
            // Return a single empty tuple (no columns)
            Ok(Some(Tuple::new(vec![])))
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::new(Schema { columns: vec![] })
    }
}

/// StatusMessage operator for DDL operations
///
/// Returns a single row with a status message, used for DDL operations
/// like CREATE FUNCTION, DROP PROCEDURE, etc.
pub struct StatusMessageOperator {
    message: String,
    exhausted: bool,
}

impl StatusMessageOperator {
    /// Create a new StatusMessage operator
    pub fn new(message: String) -> Self {
        Self { message, exhausted: false }
    }
}

impl PhysicalOperator for StatusMessageOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.exhausted {
            Ok(None)
        } else {
            self.exhausted = true;
            // Return a single tuple with the message
            Ok(Some(Tuple::new(vec![crate::Value::String(self.message.clone())])))
        }
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::new(Schema {
            columns: vec![crate::Column {
                name: "result".to_string(),
                data_type: crate::DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            }],
        })
    }
}

/// Query timeout context
///
/// Tracks query execution time and enforces timeout limits.
/// Shared across all operators in a query execution tree.
#[derive(Clone)]
pub struct TimeoutContext {
    /// Query start time
    start_time: Instant,
    /// Timeout duration (None for unlimited)
    timeout: Option<Duration>,
    /// Number of rows processed since last timeout check
    /// Used to amortize the cost of checking elapsed time
    rows_since_check: Arc<std::sync::atomic::AtomicUsize>,
}

impl TimeoutContext {
    /// Create a new timeout context
    pub fn new(timeout_ms: Option<u64>) -> Self {
        Self {
            start_time: Instant::now(),
            timeout: timeout_ms.map(Duration::from_millis),
            rows_since_check: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Check if query has exceeded timeout
    ///
    /// This check is optimized to only examine the clock every N rows
    /// to minimize performance overhead. Returns an error if timeout exceeded.
    pub fn check_timeout(&self) -> Result<()> {
        // Skip check if no timeout is set
        let timeout = match self.timeout {
            Some(t) => t,
            None => return Ok(()),
        };

        // Only check time every 1000 rows to minimize overhead
        // This amortizes the cost of Instant::now() across many rows
        const CHECK_INTERVAL: usize = 1000;
        let count = self.rows_since_check
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if count % CHECK_INTERVAL != 0 {
            return Ok(());
        }

        // Check if elapsed time exceeds timeout
        let elapsed = self.start_time.elapsed();
        if elapsed > timeout {
            return Err(Error::query_timeout(format!(
                "Query exceeded timeout limit of {}ms (elapsed: {}ms)",
                timeout.as_millis(),
                elapsed.as_millis()
            )));
        }

        Ok(())
    }

    /// Get elapsed time since query start
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }
}

/// Physical execution operator
///
/// Each operator produces tuples on demand via the `next()` method.
/// This is the core of the Volcano model.
pub trait PhysicalOperator {
    /// Get the next tuple from this operator
    ///
    /// Returns `Ok(Some(tuple))` if a tuple is available,
    /// `Ok(None)` if the operator is exhausted,
    /// `Err(error)` if an error occurs.
    fn next(&mut self) -> Result<Option<Tuple>>;

    /// Get the output schema of this operator
    fn schema(&self) -> Arc<Schema>;
}

/// Materialized CTE data
#[derive(Clone)]
pub struct CteData {
    /// CTE name
    pub name: String,
    /// Materialized tuples
    pub tuples: Vec<Tuple>,
    /// Schema of the CTE
    pub schema: Arc<Schema>,
}

/// Query executor
///
/// Converts logical plans into physical operators and executes them.
pub struct Executor<'a> {
    /// Storage engine reference
    storage: Option<&'a StorageEngine>,
    /// Timeout context for query execution
    timeout_ctx: Option<TimeoutContext>,
    /// Query parameters for parameterized queries ($1, $2, etc.)
    parameters: Vec<crate::Value>,
    /// Optional transaction context for ACID guarantees
    transaction: Option<&'a crate::storage::Transaction>,
    /// Materialized CTE results (name -> data)
    cte_context: std::collections::HashMap<String, CteData>,
}

impl<'a> Executor<'a> {
    /// Create a new executor without storage (for testing/placeholder)
    pub fn new() -> Self {
        Self {
            storage: None,
            timeout_ctx: None,
            parameters: Vec::new(),
            transaction: None,
            cte_context: std::collections::HashMap::new(),
        }
    }

    /// Create a new executor with storage
    pub fn with_storage(storage: &'a StorageEngine) -> Self {
        Self {
            storage: Some(storage),
            timeout_ctx: None,
            parameters: Vec::new(),
            transaction: None,
            cte_context: std::collections::HashMap::new(),
        }
    }

    /// Get a CTE by name if it exists in the context
    pub fn get_cte(&self, name: &str) -> Option<&CteData> {
        self.cte_context.get(name)
    }

    /// Add a CTE to the context
    pub fn add_cte(&mut self, cte: CteData) {
        self.cte_context.insert(cte.name.clone(), cte);
    }

    /// Set transaction context
    pub fn with_transaction(mut self, txn: &'a crate::storage::Transaction) -> Self {
        self.transaction = Some(txn);
        self
    }

    /// Set query timeout from configuration
    pub fn with_timeout(mut self, timeout_ms: Option<u64>) -> Self {
        self.timeout_ctx = Some(TimeoutContext::new(timeout_ms));
        self
    }

    /// Set query parameters for parameterized queries
    pub fn with_parameters(mut self, parameters: Vec<crate::Value>) -> Self {
        self.parameters = parameters;
        self
    }

    /// Execute a logical plan and return all results
    pub fn execute(&mut self, plan: &LogicalPlan) -> Result<Vec<Tuple>> {
        let build_start = Instant::now();
        let mut operator = self.plan_to_operator(plan)?;
        let build_elapsed = build_start.elapsed();
        tracing::debug!(
            phase = "operator_build",
            duration_us = build_elapsed.as_micros() as u64,
            plan_type = %plan.plan_type_name(),
            "Physical operator tree built"
        );

        let exec_start = Instant::now();
        let mut results = Vec::with_capacity(256);
        while let Some(tuple) = operator.next()? {
            results.push(tuple);
        }
        let exec_elapsed = exec_start.elapsed();
        tracing::debug!(
            phase = "operator_exec",
            duration_us = exec_elapsed.as_micros() as u64,
            rows = results.len(),
            "Operator execution complete"
        );

        Ok(results)
    }

    /// Materialize IN subqueries by executing them and converting to InList
    ///
    /// This allows the evaluator to handle IN expressions without needing
    /// access to the storage engine.
    pub(crate) fn materialize_subqueries(&self, expr: &crate::sql::LogicalExpr) -> Result<crate::sql::LogicalExpr> {
        use crate::sql::LogicalExpr;

        match expr {
            LogicalExpr::InSubquery { expr: inner_expr, subquery, negated } => {
                // Execute the subquery to get the list of values
                let mut subquery_executor = if let Some(storage) = self.storage {
                    Executor::with_storage(storage)
                } else {
                    Executor::new()
                }.with_parameters(self.parameters.clone());

                let results = subquery_executor.execute(subquery)?;

                // Materialize the inner expression as well
                let materialized_inner = self.materialize_subqueries(inner_expr)?;

                // Use HashSet for large IN lists (O(1) lookup instead of O(N) linear scan)
                if results.len() > 16 {
                    let value_set: std::collections::HashSet<crate::Value> = results.iter()
                        .filter_map(|tuple| tuple.values.first().cloned())
                        .collect();
                    Ok(LogicalExpr::InSet {
                        expr: Box::new(materialized_inner),
                        values: value_set,
                        negated: *negated,
                    })
                } else {
                    let list: Vec<LogicalExpr> = results.iter()
                        .filter_map(|tuple| {
                            tuple.values.first().map(|v| LogicalExpr::Literal(v.clone()))
                        })
                        .collect();
                    Ok(LogicalExpr::InList {
                        expr: Box::new(materialized_inner),
                        list,
                        negated: *negated,
                    })
                }
            }
            LogicalExpr::Exists { subquery, negated } => {
                // Execute the subquery to check if any rows exist
                let mut subquery_executor = if let Some(storage) = self.storage {
                    Executor::with_storage(storage)
                } else {
                    Executor::new()
                }.with_parameters(self.parameters.clone());

                let results = subquery_executor.execute(subquery)?;

                // EXISTS returns true if subquery returns any rows
                let exists = !results.is_empty();
                let result = if *negated { !exists } else { exists };

                Ok(LogicalExpr::Literal(crate::Value::Boolean(result)))
            }
            // Recursively process compound expressions
            LogicalExpr::BinaryExpr { left, op, right } => {
                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(self.materialize_subqueries(left)?),
                    op: *op,
                    right: Box::new(self.materialize_subqueries(right)?),
                })
            }
            LogicalExpr::UnaryExpr { op, expr: inner } => {
                Ok(LogicalExpr::UnaryExpr {
                    op: *op,
                    expr: Box::new(self.materialize_subqueries(inner)?),
                })
            }
            LogicalExpr::IsNull { expr: inner, is_null } => {
                Ok(LogicalExpr::IsNull {
                    expr: Box::new(self.materialize_subqueries(inner)?),
                    is_null: *is_null,
                })
            }
            LogicalExpr::Between { expr: inner, low, high, negated } => {
                Ok(LogicalExpr::Between {
                    expr: Box::new(self.materialize_subqueries(inner)?),
                    low: Box::new(self.materialize_subqueries(low)?),
                    high: Box::new(self.materialize_subqueries(high)?),
                    negated: *negated,
                })
            }
            LogicalExpr::InList { expr: inner, list, negated } => {
                let materialized_list: Result<Vec<LogicalExpr>> = list.iter()
                    .map(|e| self.materialize_subqueries(e))
                    .collect();
                Ok(LogicalExpr::InList {
                    expr: Box::new(self.materialize_subqueries(inner)?),
                    list: materialized_list?,
                    negated: *negated,
                })
            }
            LogicalExpr::Case { expr: operand, when_then, else_result } => {
                let materialized_operand = if let Some(op) = operand {
                    Some(Box::new(self.materialize_subqueries(op)?))
                } else {
                    None
                };
                let materialized_when_then: Result<Vec<(LogicalExpr, LogicalExpr)>> = when_then.iter()
                    .map(|(w, t)| Ok((self.materialize_subqueries(w)?, self.materialize_subqueries(t)?)))
                    .collect();
                let materialized_else = if let Some(e) = else_result {
                    Some(Box::new(self.materialize_subqueries(e)?))
                } else {
                    None
                };
                Ok(LogicalExpr::Case {
                    expr: materialized_operand,
                    when_then: materialized_when_then?,
                    else_result: materialized_else,
                })
            }
            // For other expressions, return as-is
            _ => Ok(expr.clone()),
        }
    }

    /// Try to use PK ART index for a point lookup when we have Filter(Scan) with `pk_col = literal`.
    /// Returns Some(operator) if successful, None if not applicable.
    fn try_index_point_lookup(
        &self,
        input: &LogicalPlan,
        predicate: &crate::sql::LogicalExpr,
    ) -> Result<Option<Box<dyn PhysicalOperator>>> {
        use crate::sql::LogicalExpr;
        use crate::sql::BinaryOperator;

        // Only works with a Scan input and storage available
        let storage = match self.storage {
            Some(s) => s,
            None => return Ok(None),
        };

        let (table_name, alias, schema, projection, as_of) = match input {
            LogicalPlan::Scan { table_name, alias, schema, projection, as_of } => {
                (table_name, alias, schema, projection, as_of)
            }
            _ => return Ok(None),
        };

        // Skip time-travel queries (need snapshot logic)
        if as_of.is_some() {
            return Ok(None);
        }

        // Skip when a transaction is active — PK index lookup reads the current
        // value from storage, bypassing MVCC snapshot isolation. The transaction
        // path in the scan operator uses scan_table_at_snapshot() instead.
        if self.transaction.is_some() {
            return Ok(None);
        }

        // Find the PK column
        let pk_col = match schema.columns.iter().find(|c| c.primary_key) {
            Some(c) => c,
            None => return Ok(None),
        };

        // Check if predicate is `pk_col = literal` or `literal = pk_col`
        let pk_value = match predicate {
            LogicalExpr::BinaryExpr { left, op: BinaryOperator::Eq, right } => {
                match (left.as_ref(), right.as_ref()) {
                    (LogicalExpr::Column { name, .. }, LogicalExpr::Literal(val))
                        if name == &pk_col.name => Some(val.clone()),
                    (LogicalExpr::Literal(val), LogicalExpr::Column { name, .. })
                        if name == &pk_col.name => Some(val.clone()),
                    // Handle parameterized query: pk_col = $1
                    (LogicalExpr::Column { name, .. }, LogicalExpr::Parameter { index })
                        if name == &pk_col.name => {
                        self.parameters.get(index.saturating_sub(1)).cloned()
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        let pk_value = match pk_value {
            Some(v) => v,
            None => return Ok(None),
        };

        // Try the ART index lookup (pass pre-fetched schema to avoid redundant catalog lookup)
        let tuple = storage.get_row_by_pk_with_schema(table_name, &pk_value, schema)?;

        // Build schema with source_table set for JOIN disambiguation
        let source_alias = alias.as_deref().unwrap_or(table_name);
        let schema_cols: Vec<_> = schema.columns.iter().map(|col| {
            let mut c = col.clone();
            c.source_table = Some(source_alias.to_string());
            c.source_table_name = Some(table_name.clone());
            c
        }).collect();
        let actual_schema = Arc::new(Schema { columns: schema_cols });

        let tuples = match tuple {
            Some(t) => vec![t],
            None => vec![],
        };

        Ok(Some(Box::new(scan::ScanOperator::new(
            table_name.clone(),
            actual_schema,
            projection.clone(),
            tuples,
            self.parameters.clone(),
        ).with_timeout(self.timeout_ctx()))))
    }

    /// Convert a logical plan to a physical operator
    pub(crate) fn plan_to_operator(&mut self, plan: &LogicalPlan) -> Result<Box<dyn PhysicalOperator>> {
        match plan {
            LogicalPlan::Scan { .. } => {
                scan::handle_scan(self, plan)
            }
            LogicalPlan::FilteredScan { .. } => {
                scan::handle_filtered_scan(self, plan)
            }
            LogicalPlan::TableFunction { .. } => {
                scan::handle_table_function(self, plan)
            }
            LogicalPlan::Filter { input, predicate } => {
                // Try PK index-based point lookup for Filter(Scan) with equality predicate
                if let Some(result) = self.try_index_point_lookup(input, predicate)? {
                    return Ok(result);
                }
                let input_op = self.plan_to_operator(input)?;
                // Materialize any IN subqueries before creating the filter
                let materialized_predicate = self.materialize_subqueries(predicate)?;
                Ok(Box::new(FilterOperator::new(
                    input_op,
                    materialized_predicate,
                    self.parameters.clone(),
                ).with_timeout(self.timeout_ctx.clone())))
            }
            LogicalPlan::Project { input, exprs, aliases, distinct, distinct_on } => {
                use crate::sql::LogicalExpr;

                // Check if any expressions are window functions
                let has_window_functions = exprs.iter().any(|e| matches!(e, LogicalExpr::WindowFunction { .. }));

                if has_window_functions {
                    let input_op = self.plan_to_operator(input)?;
                    let input_schema = input_op.schema();
                    let input_col_count = input_schema.columns.len();

                    // Collect window function expressions with their aliases
                    let mut window_exprs: Vec<(LogicalExpr, String)> = Vec::new();
                    let mut window_indices: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

                    for (i, (expr, alias)) in exprs.iter().zip(aliases.iter()).enumerate() {
                        if matches!(expr, LogicalExpr::WindowFunction { .. }) {
                            window_indices.insert(i, window_exprs.len());
                            window_exprs.push((expr.clone(), alias.clone()));
                        }
                    }

                    // Build window output schema (input + window columns)
                    let mut window_schema_cols = input_schema.columns.clone();
                    for (_, name) in &window_exprs {
                        window_schema_cols.push(crate::Column {
                            name: name.clone(),
                            data_type: crate::DataType::Int8, // Will be inferred properly at runtime
                            nullable: true,
                            primary_key: false,
                            source_table: None,
                            source_table_name: None,
                            default_expr: None,
                            unique: false,
                            storage_mode: crate::ColumnStorageMode::Default,
                        });
                    }
                    let window_schema = Arc::new(Schema { columns: window_schema_cols });

                    // Create window operator
                    let window_op = WindowOperator::new(input_op, window_exprs, window_schema);

                    // Create modified expressions that reference window columns
                    // Window function results are appended after input columns
                    let modified_exprs: Vec<LogicalExpr> = exprs
                        .iter()
                        .enumerate()
                        .map(|(i, expr)| {
                            if window_indices.contains_key(&i) {
                                // Reference the appended window column by name
                                LogicalExpr::Column {
                                    table: None,
                                    name: aliases.get(i).cloned().unwrap_or_default(),
                                }
                            } else {
                                expr.clone()
                            }
                        })
                        .collect();

                    Ok(Box::new(ProjectOperator::new_with_distinct_on(
                        Box::new(window_op),
                        modified_exprs,
                        aliases.clone(),
                        *distinct,
                        distinct_on.clone(),
                        self.parameters.clone(),
                    ).with_timeout(self.timeout_ctx.clone())))
                } else {
                    let input_op = self.plan_to_operator(input)?;
                    // Materialize any subqueries in project expressions
                    let materialized_exprs: Vec<LogicalExpr> = exprs
                        .iter()
                        .map(|e| self.materialize_subqueries(e))
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Box::new(ProjectOperator::new_with_distinct_on(
                        input_op,
                        materialized_exprs,
                        aliases.clone(),
                        *distinct,
                        distinct_on.clone(),
                        self.parameters.clone(),
                    ).with_timeout(self.timeout_ctx.clone())))
                }
            }
            LogicalPlan::Limit { input, limit, offset } => {
                // LIMIT pushdown: detect Scan or Project(Scan) with no filter/sort
                let scan_info = match input.as_ref() {
                    LogicalPlan::Scan { table_name, schema, projection, .. } => {
                        Some((table_name, schema, projection))
                    }
                    LogicalPlan::Project { input: inner, .. } => {
                        if let LogicalPlan::Scan { table_name, schema, projection, .. } = inner.as_ref() {
                            Some((table_name, schema, projection))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some((table_name, schema, projection)) = scan_info {
                    if let Some(storage) = self.storage {
                        let fetch_count = limit.saturating_add(*offset);
                        let tuples = storage.scan_table_with_limit(table_name, fetch_count)?;
                        let scan_op = Box::new(ScanOperator::new(
                            table_name.clone(), schema.clone(), projection.clone(), tuples, self.parameters.clone(),
                        ).with_timeout(self.timeout_ctx.clone()));
                        // If original input was Project(Scan), wrap with ProjectOperator
                        let final_input: Box<dyn PhysicalOperator> = if let LogicalPlan::Project { exprs, aliases, distinct, distinct_on, .. } = input.as_ref() {
                            let materialized_exprs: Vec<crate::sql::LogicalExpr> = exprs
                                .iter()
                                .map(|e| self.materialize_subqueries(e))
                                .collect::<Result<Vec<_>>>()?;
                            Box::new(ProjectOperator::new_with_distinct_on(
                                scan_op,
                                materialized_exprs,
                                aliases.clone(),
                                *distinct,
                                distinct_on.clone(),
                                self.parameters.clone(),
                            ).with_timeout(self.timeout_ctx.clone()))
                        } else {
                            scan_op
                        };
                        return Ok(Box::new(LimitOperator::new(
                            final_input,
                            *limit,
                            *offset,
                        ).with_timeout(self.timeout_ctx.clone())));
                    }
                }
                let input_op = self.plan_to_operator(input)?;
                Ok(Box::new(LimitOperator::new(
                    input_op,
                    *limit,
                    *offset,
                ).with_timeout(self.timeout_ctx.clone())))
            }
            LogicalPlan::Sort { input, exprs, asc } => {
                let input_op = self.plan_to_operator(input)?;
                Ok(Box::new(SortOperator::new(
                    input_op,
                    exprs.clone(),
                    asc.clone(),
                    self.timeout_ctx.clone(),
                )?))
            }
            LogicalPlan::Aggregate { input, group_by, aggr_exprs, having } => {
                // Fast path: COUNT(*) with no GROUP BY, no HAVING, plain Scan input
                if group_by.is_empty() && having.is_none() && aggr_exprs.len() == 1 {
                    if let crate::sql::LogicalExpr::AggregateFunction {
                        fun: crate::sql::logical_plan::AggregateFunction::Count,
                        distinct: false,
                        args,
                        ..
                    } = &aggr_exprs[0] {
                    // Only use fast path for COUNT(*), not COUNT(col)
                    // COUNT(col) needs to evaluate per-row to skip NULLs
                    let is_count_star = args.first().map_or(false, |a| matches!(a, crate::sql::LogicalExpr::Wildcard));
                    if is_count_star {
                        let scan_table = match input.as_ref() {
                            LogicalPlan::Scan { table_name, .. } => Some(table_name.as_str()),
                            LogicalPlan::Project { input: inner, .. } => {
                                if let LogicalPlan::Scan { table_name, .. } = inner.as_ref() {
                                    Some(table_name.as_str())
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some(table_name) = scan_table {
                            if let Some(storage) = self.storage {
                                let count = storage.count_table_rows(table_name)?;
                                let result_tuple = crate::Tuple::new(vec![crate::Value::Int8(count as i64)]);
                                return Ok(Box::new(MaterializedOperator::new(
                                    vec![result_tuple],
                                    count_star_schema(),
                                )));
                            }
                        }

                        // Fast path: COUNT(*) with Filter(Scan) — scan + filter + count without materializing
                        if let LogicalPlan::Filter { input: filter_input, predicate } = input.as_ref() {
                            let scan_table_filtered = match filter_input.as_ref() {
                                LogicalPlan::Scan { table_name, .. } => Some((table_name.as_str(), filter_input.as_ref())),
                                LogicalPlan::Project { input: inner, .. } => {
                                    if let LogicalPlan::Scan { table_name, .. } = inner.as_ref() {
                                        Some((table_name.as_str(), filter_input.as_ref()))
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            };
                            if let Some((table_name, scan_plan)) = scan_table_filtered {
                                if let Some(storage) = self.storage {
                                    // Build scan operator to get schema, then iterate + filter + count
                                    let mut scan_op = self.plan_to_operator(&Box::new(scan_plan.clone()))?;
                                    let schema = scan_op.schema();
                                    let evaluator = crate::sql::Evaluator::with_parameters(schema, self.parameters.clone());
                                    let _ = table_name; // used for debug context
                                    let mut count: i64 = 0;
                                    while let Some(tuple) = scan_op.next()? {
                                        if let Some(ref ctx) = self.timeout_ctx {
                                            ctx.check_timeout()?;
                                        }
                                        let result = evaluator.evaluate(predicate, &tuple)?;
                                        if matches!(result, crate::Value::Boolean(true)) {
                                            count += 1;
                                        }
                                    }
                                    let result_tuple = crate::Tuple::new(vec![crate::Value::Int8(count)]);
                                    return Ok(Box::new(MaterializedOperator::new(
                                        vec![result_tuple],
                                        count_star_schema(),
                                    )));
                                }
                            }
                        }
                    } // end if is_count_star
                    }
                }
                let input_op = self.plan_to_operator(input)?;
                Ok(Box::new(AggregateOperator::new(
                    input_op,
                    group_by.clone(),
                    aggr_exprs.clone(),
                    having.clone(),
                    self.parameters.clone(),
                    self.timeout_ctx.clone(),
                )?))
            }
            LogicalPlan::Join { left, right, join_type, on, lateral } => {
                join::handle_join(self, left, right, join_type, on, *lateral)
            }
            LogicalPlan::Union { left, right, all } => {
                let left_op = self.plan_to_operator(left)?;
                let right_op = self.plan_to_operator(right)?;
                Ok(Box::new(UnionOperator::new(left_op, right_op, *all)?))
            }
            LogicalPlan::Intersect { left, right, all } => {
                let left_op = self.plan_to_operator(left)?;
                let right_op = self.plan_to_operator(right)?;
                Ok(Box::new(IntersectOperator::new(left_op, right_op, *all)?))
            }
            LogicalPlan::Except { left, right, all } => {
                let left_op = self.plan_to_operator(left)?;
                let right_op = self.plan_to_operator(right)?;
                Ok(Box::new(ExceptOperator::new(left_op, right_op, *all)?))
            }
            LogicalPlan::CreateIndex { .. } => {
                ddl::handle_create_index(self, plan)
            }
            LogicalPlan::DropTable { name, if_exists } => {
                ddl::handle_drop_table(self, name, *if_exists)
            }
            LogicalPlan::Truncate { table_name } => {
                ddl::handle_truncate(self, table_name)
            }
            LogicalPlan::CreateBranch { .. }
            | LogicalPlan::DropBranch { .. }
            | LogicalPlan::MergeBranch { .. }
            | LogicalPlan::UseBranch { .. }
            | LogicalPlan::ShowBranches
            | LogicalPlan::CreateMaterializedView { .. }
            | LogicalPlan::RefreshMaterializedView { .. }
            | LogicalPlan::DropMaterializedView { .. }
            | LogicalPlan::AlterMaterializedView { .. }
            | LogicalPlan::CreateView { .. }
            | LogicalPlan::DropView { .. }
            | LogicalPlan::SystemView { .. } => {
                phase3::handle_phase3_operation(self, plan)
            }
            LogicalPlan::With { ctes, query, recursive } => {
                // Materialize each CTE before executing the main query
                // CTEs are stored in cte_context and looked up during table scans
                for (cte_name, cte_plan, column_aliases) in ctes {
                    // Get the plan's schema and apply column aliases if present
                    let original_schema = cte_plan.schema();
                    let cte_schema = if let Some(aliases) = column_aliases {
                        if aliases.len() == original_schema.columns.len() {
                            // Rename columns using the aliases
                            Arc::new(Schema::new(
                                original_schema.columns.iter()
                                    .zip(aliases.iter())
                                    .map(|(col, alias)| {
                                        let mut new_col = col.clone();
                                        new_col.name = alias.clone();
                                        new_col
                                    })
                                    .collect()
                            ))
                        } else {
                            original_schema
                        }
                    } else {
                        original_schema
                    };

                    if *recursive {
                        // Handle recursive CTE using iterative fixpoint evaluation
                        // The CTE plan is typically a UNION ALL of:
                        //   1. Base case (anchor term) - doesn't reference the CTE
                        //   2. Recursive case - references the CTE itself
                        //
                        // Algorithm:
                        // 1. Execute the full plan once to get initial results (base case)
                        // 2. Loop: re-execute with current results as the CTE's value
                        // 3. Stop when no new rows are produced

                        const MAX_RECURSION_DEPTH: usize = 1000;
                        let mut all_tuples: Vec<Tuple> = Vec::new();
                        let mut iteration = 0;

                        // First iteration: register empty CTE, then execute to get base results
                        self.add_cte(CteData {
                            name: cte_name.clone(),
                            tuples: vec![],
                            schema: cte_schema.clone(),
                        });

                        let mut cte_operator = self.plan_to_operator(cte_plan)?;
                        let mut new_tuples = Vec::new();
                        while let Some(tuple) = cte_operator.next()? {
                            new_tuples.push(tuple);
                        }

                        all_tuples.extend(new_tuples.clone());

                        // Iterative loop: keep re-executing with the new results
                        // until no new rows are produced (fixpoint)
                        while !new_tuples.is_empty() && iteration < MAX_RECURSION_DEPTH {
                            iteration += 1;

                            // Update the CTE with the working table (new_tuples from last iteration)
                            self.add_cte(CteData {
                                name: cte_name.clone(),
                                tuples: new_tuples.clone(),
                                schema: cte_schema.clone(),
                            });

                            // Re-execute to get next iteration's results
                            let mut cte_operator = self.plan_to_operator(cte_plan)?;
                            new_tuples.clear();
                            while let Some(tuple) = cte_operator.next()? {
                                // Only add tuples not already in all_tuples to avoid infinite loops
                                if !all_tuples.contains(&tuple) {
                                    new_tuples.push(tuple);
                                }
                            }

                            all_tuples.extend(new_tuples.clone());
                        }

                        if iteration >= MAX_RECURSION_DEPTH {
                            tracing::warn!("Recursive CTE '{}' reached maximum recursion depth {}", cte_name, MAX_RECURSION_DEPTH);
                        }

                        // Store final results
                        self.add_cte(CteData {
                            name: cte_name.clone(),
                            tuples: all_tuples,
                            schema: cte_schema,
                        });
                    } else {
                        // Non-recursive CTE: execute once and materialize
                        let mut cte_operator = self.plan_to_operator(cte_plan)?;
                        let mut tuples = Vec::new();
                        while let Some(tuple) = cte_operator.next()? {
                            tuples.push(tuple);
                        }

                        // Store the CTE in context for later lookup during scans
                        self.add_cte(CteData {
                            name: cte_name.clone(),
                            tuples,
                            schema: cte_schema,
                        });
                    }
                }

                // Now execute the main query with CTEs available in context
                self.plan_to_operator(query)
            }
            LogicalPlan::Explain { input, options } => {
                explain::handle_explain(self, input, options)
            }
            LogicalPlan::DualScan => {
                // DualScan returns a single row with no columns
                // Used as input for SELECT without FROM (e.g., SELECT 1+1)
                Ok(Box::new(DualScanOperator::new()))
            }
            // Procedural SQL statements
            LogicalPlan::CreateFunction { name, .. } => {
                // Return a status message
                let msg = format!("Function '{}' created", name);
                Ok(Box::new(StatusMessageOperator::new(msg)))
            }
            LogicalPlan::CreateProcedure { name, .. } => {
                let msg = format!("Procedure '{}' created", name);
                Ok(Box::new(StatusMessageOperator::new(msg)))
            }
            LogicalPlan::DropFunction { name, if_exists } => {
                let msg = if *if_exists {
                    format!("Function '{}' dropped (if exists)", name)
                } else {
                    format!("Function '{}' dropped", name)
                };
                Ok(Box::new(StatusMessageOperator::new(msg)))
            }
            LogicalPlan::DropProcedure { name, if_exists } => {
                let msg = if *if_exists {
                    format!("Procedure '{}' dropped (if exists)", name)
                } else {
                    format!("Procedure '{}' dropped", name)
                };
                Ok(Box::new(StatusMessageOperator::new(msg)))
            }
            LogicalPlan::Call { name, args } => {
                // For now, return a status message. Full procedure execution will be implemented later.
                let msg = format!("Procedure '{}' called with {} arguments", name, args.len());
                Ok(Box::new(StatusMessageOperator::new(msg)))
            }

            // HA Operations (ha-tier1 feature)
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::Switchover { target_node } => {
                ddl::handle_switchover(self, target_node)
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::SwitchoverCheck { target_node } => {
                ddl::handle_switchover_check(self, target_node)
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::ClusterStatus => {
                ddl::handle_cluster_status(self)
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::SetNodeAlias { node_id, alias } => {
                ddl::handle_set_node_alias(self, node_id, alias)
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::ShowTopology => {
                ddl::handle_show_topology(self)
            }

            _ => Err(Error::query_execution(format!(
                "Operator not yet implemented: {:?}",
                plan
            ))),
        }
    }

    /// Get storage engine reference (for submodules)
    pub(crate) fn storage(&self) -> Option<&StorageEngine> {
        self.storage
    }

    /// Get timeout context (for submodules)
    pub(crate) fn timeout_ctx(&self) -> Option<TimeoutContext> {
        self.timeout_ctx.clone()
    }

    /// Get query parameters (for submodules)
    pub(crate) fn parameters(&self) -> &[crate::Value] {
        &self.parameters
    }

    /// Get transaction context (for submodules)
    pub(crate) fn transaction(&self) -> Option<&'a crate::storage::Transaction> {
        self.transaction
    }
}

impl Default for Executor<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Compare two values for sorting
pub(crate) fn compare_values(a: &crate::Value, b: &crate::Value) -> std::cmp::Ordering {
    use crate::Value;
    use std::cmp::Ordering;

    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,

        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),

        (Value::Int2(a), Value::Int2(b)) => a.cmp(b),
        (Value::Int4(a), Value::Int4(b)) => a.cmp(b),
        (Value::Int8(a), Value::Int8(b)) => a.cmp(b),

        (Value::Float4(a), Value::Float4(b)) => {
            a.partial_cmp(b).unwrap_or(Ordering::Equal)
        }
        (Value::Float8(a), Value::Float8(b)) => {
            a.partial_cmp(b).unwrap_or(Ordering::Equal)
        }

        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),

        (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        // For JSON and complex types, compare as strings
        (Value::Json(a), Value::Json(b)) => {
            a.to_string().cmp(&b.to_string())
        }
        (Value::Array(a), Value::Array(b)) => {
            // Lexicographic array comparison
            a.len().cmp(&b.len()).then_with(|| {
                for (val_a, val_b) in a.iter().zip(b.iter()) {
                    let cmp = compare_values(val_a, val_b);
                    if cmp != Ordering::Equal {
                        return cmp;
                    }
                }
                Ordering::Equal
            })
        }
        (Value::Vector(a), Value::Vector(b)) => {
            // Compare vector length first, then lexicographically
            a.len().cmp(&b.len()).then_with(|| {
                for (val_a, val_b) in a.iter().zip(b.iter()) {
                    let cmp = val_a.partial_cmp(val_b).unwrap_or(Ordering::Equal);
                    if cmp != Ordering::Equal {
                        return cmp;
                    }
                }
                Ordering::Equal
            })
        }

        // Different types - order by type priority
        _ => {
            fn type_priority(val: &Value) -> u8 {
                match val {
                    Value::Null => 0,
                    Value::Boolean(_) => 1,
                    Value::Int2(_) => 2,
                    Value::Int4(_) => 3,
                    Value::Int8(_) => 4,
                    Value::Float4(_) => 5,
                    Value::Float8(_) => 6,
                    Value::Numeric(_) => 7,
                    Value::String(_) => 8,
                    Value::Bytes(_) => 9,
                    Value::Uuid(_) => 10,
                    Value::Timestamp(_) => 11,
                    Value::Date(_) => 12,
                    Value::Time(_) => 13,
                    Value::Json(_) => 14,
                    Value::Array(_) => 15,
                    Value::Vector(_) => 16,
                    // Storage references (shouldn't normally appear in user data)
                    Value::DictRef { .. } => 17,
                    Value::CasRef { .. } => 18,
                    Value::ColumnarRef => 19,
                    Value::Interval(_) => 20, // Interval type
                }
            }
            type_priority(a).cmp(&type_priority(b))
        }
    }
}
