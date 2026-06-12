//! Volcano-model query executor
//!
//! This module implements a simple iterator-based query execution engine
//! using the Volcano model (also known as the iterator model or pipeline model).
//!
//! Each operator implements a simple interface:
//! - `next()` - returns the next tuple or None when exhausted
//!
//! Operators are composed into a tree that processes data one tuple at a time.

use crate::sql::LogicalPlan;
use crate::storage::StorageEngine;
use crate::{Error, Result, Schema, Tuple};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Re-export submodules
pub mod aggregate;
pub mod ddl;
pub mod explain;
pub mod filter;
pub mod join;
pub mod phase3;
pub mod project;
pub mod scan;
pub mod set_ops;
pub mod topk;
pub mod window;

// Re-export operators for public API
pub use aggregate::{AggregateOperator, SortOperator};
pub use filter::FilterOperator;
pub use join::{HashJoinOperator, NestedLoopJoinOperator};
pub use project::{LimitOperator, ProjectOperator};
pub use scan::{GenerateSeriesOperator, MaterializedOperator, ScanOperator, UnnestOperator, VectorScanOperator};
pub use set_ops::{ExceptOperator, IntersectOperator, UnionOperator};
pub use topk::TopKOperator;
pub use window::WindowOperator;

type IntRangeBounds = (Option<(i64, bool)>, Option<(i64, bool)>);

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

/// Coerce a SQL literal Value to a column's declared type when
/// the obvious cross-type case calls for it.  Currently handles
/// String→UUID/Date/Timestamp; everything else passes through.
///
/// Necessary because the planner emits `Value::String(...)` for
/// any quoted literal regardless of the comparison column's type,
/// and the ART index lookup encodes types byte-exactly.  Without
/// this coercion `WHERE id = '<uuid>'` against a UUID PK misses
/// every row.
pub(crate) fn coerce_literal_to_column_type(v: crate::Value, col_type: &crate::DataType) -> crate::Value {
    use crate::{DataType, Value};
    match (&v, col_type) {
        (Value::String(s), DataType::Uuid) => match uuid::Uuid::parse_str(s) {
            Ok(u) => Value::Uuid(u),
            Err(_) => v,
        },
        (Value::String(s), DataType::Date) => match s.parse::<chrono::NaiveDate>() {
            Ok(d) => Value::Date(d),
            Err(_) => v,
        },
        (Value::String(s), DataType::Timestamp) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(t) => Value::Timestamp(t.to_utc()),
            Err(_) => v,
        },
        _ => v,
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
        Self {
            message,
            exhausted: false,
        }
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
        let count = self.rows_since_check.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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
    /// Materialized tuples, `Arc`-shared so each CTE reference serves the
    /// same materialization instead of deep-cloning it (R3.5 item 5)
    pub tuples: Arc<Vec<Tuple>>,
    /// Schema of the CTE
    pub schema: Arc<Schema>,
}

struct DirectTopKProjectSpec {
    table_name: String,
    scan_schema: Arc<Schema>,
    output_schema: Arc<Schema>,
    output_columns: Vec<usize>,
    sort_columns: Vec<usize>,
}

fn direct_expr_column_index(schema: &Schema, expr: &crate::sql::LogicalExpr) -> Option<usize> {
    match expr {
        crate::sql::LogicalExpr::Column { table, name } => schema
            .get_qualified_column_index(table.as_deref(), name)
            .or_else(|| schema.get_column_index(name)),
        _ => None,
    }
}

/// Coerce a resolved constant `Value` into a query vector for kNN search.
///
/// Accepts the same shapes the vector-distance evaluator does:
///   * `Value::Vector` — already a vector
///   * `Value::Array` of numerics — e.g. an array literal bound as a param
///   * `Value::String` in pgvector `[1,2,3]` text form
///
/// Returns `None` for anything that isn't vector-shaped so the caller falls
/// back to the brute-force scan rather than mis-answering the query.
fn value_to_query_vector(value: &crate::Value) -> Option<Vec<f32>> {
    use crate::Value;
    match value {
        Value::Vector(v) => Some(v.clone()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(value_to_f32(item)?);
            }
            Some(out)
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
                return None;
            }
            let inner = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            if inner.is_empty() {
                return Some(Vec::new());
            }
            let mut out = Vec::new();
            for elem in inner.split(',') {
                out.push(elem.trim().parse::<f32>().ok()?);
            }
            Some(out)
        }
        _ => None,
    }
}

fn value_to_f32(value: &crate::Value) -> Option<f32> {
    use crate::Value;
    match value {
        Value::Float4(f) => Some(*f),
        Value::Float8(f) => Some(*f as f32),
        Value::Int2(i) => Some(*i as f32),
        Value::Int4(i) => Some(*i as f32),
        Value::Int8(i) => Some(*i as f32),
        Value::Numeric(s) | Value::String(s) => s.trim().parse::<f32>().ok(),
        _ => None,
    }
}

fn resolve_sort_columns_to_base(
    sort_exprs: &[crate::sql::LogicalExpr],
    output_schema: &Schema,
    output_columns: &[usize],
) -> Option<Vec<usize>> {
    let mut sort_columns = Vec::with_capacity(sort_exprs.len());
    for expr in sort_exprs {
        let output_idx = direct_expr_column_index(output_schema, expr)?;
        sort_columns.push(*output_columns.get(output_idx)?);
    }
    Some(sort_columns)
}

/// R4.4: detection result for the ordered-index top-k fast path
/// (`ORDER BY indexed_col ASC LIMIT k` → ordered index iteration, no sort).
/// Produced by [`Executor::index_ordered_topk_detect`] without executing
/// anything; consumed by the executor fast path (which then iterates) and by
/// the EXPLAIN annotator (display only).
pub(super) struct OrderedTopkSpec<'p> {
    table_name: &'p String,
    alias: &'p Option<String>,
    schema: &'p Arc<Schema>,
    projection: &'p Option<Vec<usize>>,
    /// Raw (unmaterialized) scan predicate, re-applied residually at
    /// execution time after subquery materialization.
    predicate: Option<&'p crate::sql::LogicalExpr>,
    /// `Project(Sort(..))` wrapper parameters to re-wrap around the output.
    #[allow(clippy::type_complexity)]
    project_wrap: Option<(
        Vec<crate::sql::LogicalExpr>,
        Vec<String>,
        bool,
        Option<Vec<crate::sql::LogicalExpr>>,
    )>,
    /// Sort column's index in the scan schema.
    col_idx: usize,
    pub(super) column_name: String,
    pub(super) index_name: String,
    /// Total index entries (== table row count, per the completeness gate).
    entry_count: u64,
    /// Encoded iteration bounds from a range predicate on the sort column.
    lower: Option<(Vec<u8>, bool)>,
    upper: Option<(Vec<u8>, bool)>,
}

impl OrderedTopkSpec<'_> {
    /// Table the ordered iteration scans (for EXPLAIN display).
    pub(super) fn table_name(&self) -> &str {
        self.table_name
    }
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
    /// Row-decode hints, computed once per top-level `execute` /
    /// `execute_with_columns`: scans of a listed table need only materialize the
    /// columns the plan can read. Missing table = full decode.
    scan_decode_hints: Vec<(String, scan::ScanDecodeHint)>,
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
            scan_decode_hints: Vec::new(),
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
            scan_decode_hints: Vec::new(),
        }
    }

    /// Row-decode hint for `table` if the current plan's needed-column analysis was
    /// certain enough to apply it. See `scan::compute_scan_decode_hints`.
    pub(crate) fn scan_decode_hint_for(&self, table: &str) -> Option<&scan::ScanDecodeHint> {
        self.scan_decode_hints
            .iter()
            .find_map(|(t, hint)| (t == table).then_some(hint))
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
        self.scan_decode_hints = scan::compute_scan_decode_hints(plan);
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

    /// Execute a plan and return both tuples and output column names.
    pub fn execute_with_columns(&mut self, plan: &LogicalPlan) -> Result<(Vec<Tuple>, Vec<String>)> {
        self.scan_decode_hints = scan::compute_scan_decode_hints(plan);
        let mut operator = self.plan_to_operator(plan)?;
        let columns: Vec<String> = operator.schema().columns.iter().map(|c| c.name.clone()).collect();
        let mut results = Vec::with_capacity(256);
        while let Some(tuple) = operator.next()? {
            results.push(tuple);
        }
        Ok((results, columns))
    }

    /// Pattern-match the input to a `Limit` for the Top-K optimisation:
    /// `Sort(inner)` or `Project(Sort(inner))`. Returns the sort exprs,
    /// ASC flags, the sort's inner plan, and optionally the Project
    /// parameters that need to be re-wrapped around the TopK output.
    #[allow(clippy::type_complexity)]
    fn extract_sort_for_topk(
        input: &LogicalPlan,
    ) -> Option<(
        Vec<crate::sql::LogicalExpr>,
        Vec<bool>,
        &LogicalPlan,
        Option<(
            Vec<crate::sql::LogicalExpr>,
            Vec<String>,
            bool,
            Option<Vec<crate::sql::LogicalExpr>>,
        )>,
    )> {
        match input {
            LogicalPlan::Sort {
                input: inner,
                exprs,
                asc,
            } => Some((exprs.clone(), asc.clone(), inner.as_ref(), None)),
            LogicalPlan::Project {
                input: inner,
                exprs: p_exprs,
                aliases,
                distinct,
                distinct_on,
                ..
            } => {
                if let LogicalPlan::Sort {
                    input: inner2,
                    exprs,
                    asc,
                } = inner.as_ref()
                {
                    Some((
                        exprs.clone(),
                        asc.clone(),
                        inner2.as_ref(),
                        Some((p_exprs.clone(), aliases.clone(), *distinct, distinct_on.clone())),
                    ))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn try_storage_direct_topk(
        &self,
        input: &LogicalPlan,
        limit: usize,
        offset: usize,
    ) -> Result<Option<Box<dyn PhysicalOperator>>> {
        // R2.3: transaction check is per-table once the scan target is known.
        if limit == usize::MAX || self.txn_forces_slow_reads() {
            return Ok(None);
        }
        let Some(storage) = self.storage else {
            return Ok(None);
        };
        let LogicalPlan::Sort {
            input: sort_input,
            exprs: sort_exprs,
            asc,
        } = input
        else {
            return Ok(None);
        };
        if sort_exprs.is_empty() || sort_exprs.len() != asc.len() {
            return Ok(None);
        }

        if let Some(spec) = self.direct_topk_project_spec(sort_input, sort_exprs)? {
            if self.txn_forces_slow_reads_for_table(&spec.table_name) {
                return Ok(None);
            }
            let tuples = if let Some(tuples) = storage.scan_table_topk_projected_columns(
                &spec.table_name,
                &spec.scan_schema,
                &spec.output_columns,
                &spec.sort_columns,
                asc,
                limit.saturating_add(offset),
            )? {
                tuples
            } else if let Some(tuples) = storage.scan_table_topk_columnar_projected_columns(
                &spec.table_name,
                &spec.scan_schema,
                &spec.output_columns,
                &spec.sort_columns,
                asc,
                limit.saturating_add(offset),
            )? {
                tuples
            } else {
                return Ok(None);
            };
            let input: Box<dyn PhysicalOperator> = Box::new(MaterializedOperator::new(tuples, spec.output_schema));
            return Ok(Some(Box::new(
                LimitOperator::new(input, limit, offset).with_timeout(self.timeout_ctx.clone()),
            )));
        }

        Ok(None)
    }

    /// R4.4: detection half of the ordered-index top-k fast path — decides
    /// whether `ORDER BY indexed_col ASC LIMIT k` (the `Limit` node's input)
    /// will be served by ordered index iteration, WITHOUT executing anything:
    /// no subquery materialization, no row fetches. Shared by the executor
    /// fast path and the EXPLAIN annotator so the displayed plan is the
    /// executed plan.
    ///
    /// Bounds are derived from the RAW predicate (only literals, parameters,
    /// and casts qualify — `scan::lookup_bound_value`); a predicate whose
    /// only range bound on the sort column is a subquery therefore falls
    /// back to the generic top-k path instead of being materialized here.
    pub(super) fn index_ordered_topk_detect<'p>(&self, input: &'p LogicalPlan) -> Result<Option<OrderedTopkSpec<'p>>> {
        use crate::sql::LogicalExpr;

        if scan::index_range_fast_path_disabled() {
            return Ok(None);
        }
        let Some(storage) = self.storage else {
            return Ok(None);
        };
        if storage.is_branch_active() {
            return Ok(None);
        }

        let Some((sort_exprs, sort_asc, sort_input, project_wrap)) = Self::extract_sort_for_topk(input) else {
            return Ok(None);
        };
        // Single ascending key only; DESC needs reverse iteration (future work).
        if sort_exprs.len() != 1 || sort_asc.len() != 1 || !sort_asc[0] {
            return Ok(None);
        }
        if let Some((_, _, distinct, distinct_on)) = &project_wrap {
            if *distinct || distinct_on.is_some() {
                return Ok(None);
            }
        }
        let LogicalExpr::Column { name: sort_column, .. } = &sort_exprs[0] else {
            return Ok(None);
        };

        // Underlying plan: a (possibly filtered) scan of a real table.
        let (table_name, alias, schema, projection, as_of, predicate): (
            &String,
            &Option<String>,
            &Arc<Schema>,
            &Option<Vec<usize>>,
            _,
            Option<&LogicalExpr>,
        ) = match sort_input {
            LogicalPlan::Scan {
                table_name,
                alias,
                schema,
                projection,
                as_of,
            } => (table_name, alias, schema, projection, as_of, None),
            LogicalPlan::FilteredScan {
                table_name,
                alias,
                schema,
                projection,
                predicate,
                as_of,
            } => (table_name, alias, schema, projection, as_of, predicate.as_ref()),
            LogicalPlan::Filter { input, predicate } => {
                if let LogicalPlan::Scan {
                    table_name,
                    alias,
                    schema,
                    projection,
                    as_of,
                } = input.as_ref()
                {
                    (table_name, alias, schema, projection, as_of, Some(predicate))
                } else {
                    return Ok(None);
                }
            }
            _ => return Ok(None),
        };
        if as_of.is_some() {
            return Ok(None);
        }

        // Cheap in-memory disqualifiers first (column type, index presence);
        // the catalog/MV gates below involve storage reads.
        let Some(col_idx) = schema.get_column_index(sort_column) else {
            return Ok(None);
        };
        let Some(column) = schema.columns.get(col_idx) else {
            return Ok(None);
        };
        if !scan::range_scannable_type(&column.data_type) {
            return Ok(None);
        }
        let art = storage.art_indexes();
        let Some(index_name) = art.find_column_index(table_name, &column.name) else {
            return Ok(None);
        };

        if self.txn_forces_slow_reads_for_table(table_name)
            || self.get_cte(table_name).is_some()
            || storage.mv_catalog().view_exists(table_name)?
            || !storage.catalog().table_exists(table_name)?
        {
            return Ok(None);
        }

        // Completeness gate: ordered iteration replaces the sort, so the
        // index must cover EVERY row (including NULL keys). Rows missing
        // from the index (e.g. tuples predating an ALTER TABLE ADD COLUMN)
        // would silently vanish — fall back instead.
        let Some(entry_count) = art.index_entry_count(&index_name) else {
            return Ok(None);
        };
        let table_rows = storage.count_table_rows(table_name)? as u64;
        if entry_count != table_rows {
            return Ok(None);
        }

        // Optional WHERE: a range on the sort column bounds the iteration;
        // everything (including the range itself) is re-applied residually
        // at execution time.
        let (lower, upper) = match predicate {
            Some(pred) => {
                match scan::indexed_range_lookup(storage, table_name, schema.as_ref(), pred, &self.parameters) {
                    Some(spec) if spec.column_name == column.name => (spec.lower, spec.upper),
                    // Predicate is not a range on the sort column: the
                    // residual filter could discard arbitrarily many rows
                    // per index step — let the generic top-k path handle it.
                    _ => return Ok(None),
                }
            }
            None => (None, None),
        };

        Ok(Some(OrderedTopkSpec {
            table_name,
            alias,
            schema,
            projection,
            predicate,
            project_wrap,
            col_idx,
            column_name: column.name.clone(),
            index_name,
            entry_count,
            lower,
            upper,
        }))
    }

    /// R4.4: `ORDER BY indexed_col ASC LIMIT k` served by ordered index
    /// iteration — no sort. Handles `Sort(Scan)` and `Project(Sort(Scan))`
    /// (non-distinct), optionally with a WHERE clause: a range predicate on
    /// the sort column bounds the index iteration, anything else is applied
    /// as a residual filter while iterating in index order. NULL sort keys
    /// honour the engine's ASC semantics (`compare_values` sorts NULL below
    /// every value, i.e. NULLS FIRST). DESC falls through to the generic
    /// top-k path. Detection lives in [`Self::index_ordered_topk_detect`].
    fn try_index_ordered_topk(
        &self,
        input: &LogicalPlan,
        limit: usize,
        offset: usize,
    ) -> Result<Option<Box<dyn PhysicalOperator>>> {
        let Some(spec) = self.index_ordered_topk_detect(input)? else {
            return Ok(None);
        };
        let Some(storage) = self.storage else {
            return Ok(None);
        };
        let art = storage.art_indexes();
        let OrderedTopkSpec {
            table_name,
            alias,
            schema,
            projection,
            predicate,
            project_wrap,
            col_idx,
            column_name,
            index_name,
            entry_count,
            lower,
            upper,
        } = spec;

        // Residual filter: the FULL (materialized) predicate is re-applied
        // per row, so bound semantics stay identical to the generic path.
        let materialized_predicate = predicate.map(|p| self.materialize_subqueries(p)).transpose()?;

        let source_name = alias.as_ref().unwrap_or(table_name);
        let actual_schema = Arc::new(scan::schema_with_source(schema.as_ref(), source_name, table_name));
        let residual = materialized_predicate.as_ref().map(|pred| {
            let evaluator = crate::sql::Evaluator::with_parameters(actual_schema.clone(), self.parameters.clone());
            let bound = evaluator.bind(pred.clone());
            (evaluator, bound)
        });

        let k_target = limit.saturating_add(offset);
        let total = entry_count as usize;
        let mut fetch_k = k_target.saturating_mul(2).max(k_target.saturating_add(8)).min(total);
        let mut non_null: Vec<Tuple>;
        let mut null_head: Vec<Tuple>;
        loop {
            non_null = Vec::with_capacity(k_target.min(fetch_k));
            null_head = Vec::new();
            // The engine's `compare_values` sorts NULL below every value, so
            // ASC means NULLS FIRST. NULL keys encode as the 1-byte 0x00 and
            // cluster at the front of the index; only the empty string (and
            // a literal "\0") can share that region, so all NULL rows have
            // been seen once a key > [0x00] goes by. Rows are classified by
            // the actual tuple value (never by key) to keep "\0"/"" exact.
            let mut nulls_complete = false;
            let pairs = art.index_range_scan(
                &index_name,
                lower.as_ref().map(|(key, inclusive)| (key.as_slice(), *inclusive)),
                upper.as_ref().map(|(key, inclusive)| (key.as_slice(), *inclusive)),
                Some(fetch_k),
            );
            let exhausted = pairs.len() < fetch_k;
            for (key, row_id) in &pairs {
                if key.as_slice() > [0u8].as_slice() {
                    nulls_complete = true;
                }
                let Some(tuple) = storage.get_row_by_id(table_name, *row_id, schema.as_ref())? else {
                    continue;
                };
                if let Some((evaluator, pred)) = &residual {
                    match evaluator.evaluate(pred, &tuple)? {
                        crate::Value::Boolean(true) => {}
                        crate::Value::Boolean(false) | crate::Value::Null => continue,
                        other => {
                            return Err(Error::query_execution(format!(
                                "Filter predicate must evaluate to boolean, got: {:?}",
                                other
                            )));
                        }
                    }
                }
                if matches!(tuple.values.get(col_idx), Some(crate::Value::Null) | None) {
                    null_head.push(tuple);
                    continue;
                }
                non_null.push(tuple);
                if nulls_complete && null_head.len() + non_null.len() >= k_target {
                    break;
                }
            }
            // Enough rows only counts as done once every NULL row is in hand
            // (`nulls_complete`), or when NULLs alone already fill the top-k:
            // empty-string keys sort BEFORE the NULL key, so a batch can hit
            // `k_target` on ""-rows while NULL rows (which precede them in
            // the output) are still unfetched beyond `fetch_k`.
            if exhausted
                || fetch_k >= total
                || (null_head.len() + non_null.len() >= k_target
                    && (nulls_complete || null_head.len() >= k_target))
            {
                break;
            }
            fetch_k = fetch_k.saturating_mul(4).min(total);
        }
        // NULLS FIRST (engine ASC semantics), then values in index order.
        let mut ordered = null_head;
        ordered.append(&mut non_null);
        ordered.truncate(k_target);

        tracing::debug!(
            "ordered index top-k: '{}' on {}.{} served {} of {} requested rows (no sort)",
            index_name,
            table_name,
            column_name,
            ordered.len(),
            k_target,
        );

        let scan_op: Box<dyn PhysicalOperator> = Box::new(
            scan::ScanOperator::new(
                table_name.clone(),
                actual_schema,
                projection.clone(),
                ordered,
                self.parameters.clone(),
            )
            .with_timeout(self.timeout_ctx.clone()),
        );
        let after_project: Box<dyn PhysicalOperator> = match project_wrap {
            Some((exprs, aliases, _, _)) => {
                let materialised: Vec<crate::sql::LogicalExpr> = exprs
                    .iter()
                    .map(|e| self.materialize_subqueries(e))
                    .collect::<Result<Vec<_>>>()?;
                Box::new(
                    ProjectOperator::new(scan_op, materialised, aliases, false, self.parameters.clone())
                        .with_timeout(self.timeout_ctx.clone()),
                )
            }
            None => scan_op,
        };
        Ok(Some(Box::new(
            LimitOperator::new(after_project, limit, offset).with_timeout(self.timeout_ctx.clone()),
        )))
    }

    /// Vector kNN fast path: `... ORDER BY col <distance-op> $const LIMIT k`.
    ///
    /// Detects the pgvector kNN idiom and, when an HNSW index exists on the
    /// sorted vector column whose metric matches the distance operator,
    /// answers the query out of the index instead of brute-force scanning
    /// every row. Returns `None` (so the caller falls back to the generic
    /// scan/top-k path) whenever the shape doesn't match or no suitable
    /// index is present — non-indexed kNN must NOT regress.
    ///
    /// Plan shapes handled (mirrors `place_order_by` in the planner; the
    /// optional Project may be absent in each):
    ///   * `Project(Sort(Scan))`          — the common `SELECT id ... ORDER BY emb <=> $1` case
    ///   * `Project(Sort(Filter(Scan)))`  — R5.V4: WHERE kept at executor level
    ///   * `Project(Sort(FilteredScan))`  — R5.V4: WHERE pushed to storage
    ///   * `Project(Sort(Filter(FilteredScan)))` — R5.V4: split conjuncts
    ///
    /// The Sort must have a single ascending key `col <op> const`. We pull
    /// `limit + offset` neighbours from the index, load the full tuples by
    /// row_id, post-apply the WHERE predicate when one is present
    /// (escalating the over-fetch while matches run short — see
    /// `knn_scan_with_filter`), emit survivors already ordered by ascending
    /// distance through a `VectorScanOperator`, re-apply any Project, and
    /// cap with the outer `LimitOperator` (which applies the `offset` skip).
    fn try_vector_knn_topk(
        &self,
        input: &LogicalPlan,
        limit: usize,
        offset: usize,
    ) -> Result<Option<Box<dyn PhysicalOperator>>> {
        use crate::sql::{BinaryOperator, LogicalExpr};

        // Diagnostic / testing kill switch: forces the brute-force scan+sort
        // fallback so results can be compared against the index-served path.
        if std::env::var_os("HELIOS_KNN_FAST_OFF").is_some() {
            return Ok(None);
        }
        // R2.3: transaction check is per-table once the scan target is known.
        if limit == usize::MAX || self.txn_forces_slow_reads() {
            return Ok(None);
        }
        let Some(storage) = self.storage else {
            return Ok(None);
        };

        // Peel an optional non-distinct Project off the top, remembering it so
        // we can re-wrap the indexed scan with it afterwards.
        let (project_wrap, sort_plan): (Option<(&[LogicalExpr], &[String])>, &LogicalPlan) = match input {
            LogicalPlan::Sort { .. } => (None, input),
            LogicalPlan::Project {
                input: inner,
                exprs,
                aliases,
                distinct: false,
                distinct_on: None,
            } if matches!(inner.as_ref(), LogicalPlan::Sort { .. }) => {
                (Some((exprs.as_slice(), aliases.as_slice())), inner.as_ref())
            }
            _ => return Ok(None),
        };

        let LogicalPlan::Sort {
            input: sort_input,
            exprs: sort_exprs,
            asc,
        } = sort_plan
        else {
            return Ok(None);
        };

        // Single ascending sort key only. (kNN is always nearest-first; a
        // DESC distance sort wants the *farthest* rows, which the HNSW index
        // can't answer cheaply — let it fall through to the brute-force path.)
        if sort_exprs.len() != 1 || asc.len() != 1 || !asc[0] {
            return Ok(None);
        }
        let LogicalExpr::BinaryExpr { left, op, right } = &sort_exprs[0] else {
            return Ok(None);
        };
        let metric = match op {
            BinaryOperator::VectorCosineDistance => crate::vector::DistanceMetric::Cosine,
            BinaryOperator::VectorL2Distance => crate::vector::DistanceMetric::L2,
            BinaryOperator::VectorInnerProduct => crate::vector::DistanceMetric::InnerProduct,
            _ => return Ok(None),
        };

        // The underlying plan must be a (possibly filtered) scan of a real
        // table. R5.V4: a simple WHERE clause no longer disqualifies the
        // fast path — ANN candidates are post-filtered below.
        let Some((table_name, scan_schema, post_predicate)) = self.knn_scan_with_filter(sort_input)? else {
            return Ok(None);
        };
        // R2.3: kNN out of the HNSW index is allowed inside a transaction only
        // for ReadCommitted session txns with no staged writes on this table
        // (HNSW, like ART, only reflects this txn's writes at commit).
        if self.txn_forces_slow_reads_for_table(&table_name) {
            return Ok(None);
        }

        // Identify which operand is the indexed column and which is the query
        // vector. pgvector writes `col <=> $1`, but `$1 <=> col` is equally
        // valid and distance is symmetric for all three metrics.
        let column_name = |expr: &LogicalExpr| -> Option<String> {
            if let LogicalExpr::Column { name, .. } = expr {
                if scan_schema.get_column_index(name).is_some() {
                    return Some(name.clone());
                }
            }
            None
        };
        let (col_name, query_expr): (String, &LogicalExpr) = if let Some(name) = column_name(left) {
            (name, right.as_ref())
        } else if let Some(name) = column_name(right) {
            (name, left.as_ref())
        } else {
            return Ok(None);
        };

        // The query side must be a constant we can resolve right now (literal
        // or bound parameter) — not another column, which would make this a
        // row-dependent distance the index can't serve.
        let query_vec = match self.resolve_const_query_vector(query_expr)? {
            Some(v) => v,
            None => return Ok(None),
        };

        // Find an HNSW index on (table, column) whose metric matches the
        // operator. A cosine index can't answer an L2 ORDER BY correctly.
        let vector_indexes = storage.vector_indexes();
        let mut chosen: Option<String> = None;
        for index_name in vector_indexes.find_indexes(&table_name, &col_name) {
            if let Ok(meta) = vector_indexes.get_metadata(&index_name) {
                if meta.distance_metric() == metric && meta.dimension() == query_vec.len() {
                    chosen = Some(index_name);
                    break;
                }
            }
        }
        let Some(index_name) = chosen else {
            return Ok(None);
        };

        // Pull k = limit + offset neighbours, already ordered ascending by
        // distance, then load the full tuples by row_id.
        //
        // R5.V5: deletes leave tombstones in the HNSW graph (hnsw_rs cannot
        // physically remove entries) and rows can also vanish without index
        // maintenance (TRUNCATE, restored data dirs). Both would shrink a
        // plain k-fetch below LIMIT. Over-fetch with a margin, drop dead
        // row_ids (tombstone-filtered by the index, or `get_row_by_id` →
        // None), and if still short retry once at the index's full physical
        // size — so deletes never starve the LIMIT while live rows remain.
        let k_target = limit.saturating_add(offset);
        // Small indexes are served EXACTLY by the brute-force scan fallback:
        // tiny hnsw graphs (bulk-built or tombstone-heavy) can miss live
        // nodes regardless of ef_search, and an exact scan over <=256 rows
        // is microseconds anyway.
        const SMALL_INDEX_EXACT_THRESHOLD: usize = 256;
        if vector_indexes
            .index_live_size(&index_name)
            .is_none_or(|live| live <= SMALL_INDEX_EXACT_THRESHOLD)
        {
            return Ok(None);
        }
        let physical_size = vector_indexes
            .index_physical_size(&index_name)
            .unwrap_or(k_target)
            .max(k_target);
        let mut fetch_k = k_target
            .saturating_mul(2)
            .max(k_target.saturating_add(16))
            .min(physical_size);

        // R5.V4 post-filter: evaluate the WHERE predicate on each candidate
        // tuple with the same evaluator `FilterOperator` uses, so semantics
        // (NULL handling, coercions) are identical to the brute-force path.
        let post_filter = post_predicate.map(|pred| {
            let evaluator = crate::sql::Evaluator::with_parameters(scan_schema.clone(), self.parameters.clone());
            let bound = evaluator.bind(pred);
            (evaluator, bound)
        });
        // The filtered path only serves answers found by a strict-subset
        // over-fetch (see the escalation note in the loop); without headroom
        // between the base fetch and the index size there is nothing to
        // over-fetch from, so let the brute-force path handle it.
        if post_filter.is_some() && fetch_k >= physical_size {
            return Ok(None);
        }

        let mut results: Vec<(u64, f32)> = Vec::new();
        let mut tuples: Vec<Tuple> = Vec::new();
        // Candidate count of the previous round: when a wider fetch stops
        // producing more candidates the graph search has saturated —
        // hnsw_rs's beam can terminate well below the requested k on
        // unfavourable topologies — and escalating further cannot surface
        // anything new.
        let mut prev_candidates = 0usize;
        loop {
            results.clear();
            tuples.clear();
            let neighbours = vector_indexes.search(&index_name, &query_vec, fetch_k)?;
            let candidates = neighbours.len();
            for (row_id, distance) in neighbours {
                let Some(tuple) = storage.get_row_by_id(&table_name, row_id, scan_schema.as_ref())? else {
                    continue;
                };
                if let Some((evaluator, pred)) = &post_filter {
                    match evaluator.evaluate(pred, &tuple)? {
                        crate::Value::Boolean(true) => {}
                        crate::Value::Boolean(false) | crate::Value::Null => continue,
                        other => {
                            // Same error FilterOperator raises for the
                            // brute-force path: behaviour stays identical.
                            return Err(Error::query_execution(format!(
                                "Filter predicate must evaluate to boolean, got: {:?}",
                                other
                            )));
                        }
                    }
                }
                results.push((row_id, distance));
                tuples.push(tuple);
                if results.len() >= k_target {
                    break;
                }
            }
            if results.len() >= k_target {
                break;
            }
            match &post_filter {
                // R5.V4 selectivity guard: escalate the over-fetch while the
                // filter keeps fewer than k rows, but only while a wider
                // round can actually produce more candidates AND stays a
                // strict subset of the index. A full-size graph search is
                // NOT exact — HNSW recall misses nodes even at
                // k = physical_size — so the "fetch everything" round could
                // silently drop matching rows precisely when the filter is
                // selective and every match counts. Saturated or exhausted
                // queries are handed back to the brute-force scan+sort
                // path, which is exact by construction.
                Some(_) => {
                    if candidates <= prev_candidates {
                        return Ok(None);
                    }
                    let next = if results.is_empty() {
                        // No selectivity signal yet — escalate blind.
                        fetch_k.saturating_mul(4)
                    } else {
                        // Selectivity-aware: `results.len()` of `candidates`
                        // matched, so ~`k_target * candidates / matches`
                        // candidates should contain k matches. Take a 2x
                        // safety margin, and never less than doubling, so
                        // one more round usually settles it.
                        candidates
                            .saturating_mul(k_target)
                            .checked_div(results.len())
                            .unwrap_or(usize::MAX)
                            .saturating_mul(2)
                            .max(fetch_k.saturating_mul(2))
                    };
                    if next >= physical_size {
                        return Ok(None);
                    }
                    prev_candidates = candidates;
                    fetch_k = next;
                }
                // Unfiltered misses are tombstone-driven and rare — one
                // retry at the full physical size settles them (pre-V4
                // behaviour, V5 semantics).
                None => {
                    if fetch_k >= physical_size {
                        // The full-size round still came up short. A short
                        // result is correct when the index genuinely holds
                        // fewer live rows than the LIMIT — but when more
                        // live rows exist the graph search saturated, and
                        // returning the truncated set would silently drop
                        // rows the brute-force path finds. Fall back.
                        if vector_indexes
                            .index_live_size(&index_name)
                            .is_some_and(|live| live > results.len())
                        {
                            return Ok(None);
                        }
                        break;
                    }
                    fetch_k = physical_size;
                }
            }
        }

        tracing::debug!(
            "vector kNN fast path: index '{}' on {}.{} ({:?}) served {} of {} requested neighbours (fetch_k {})",
            index_name,
            table_name,
            col_name,
            metric,
            tuples.len(),
            k_target,
            fetch_k,
        );

        let scan: Box<dyn PhysicalOperator> = Box::new(VectorScanOperator::new(
            table_name.clone(),
            scan_schema.clone(),
            results,
            tuples,
        ));

        // Re-apply the Project we peeled off, if any.
        let after_project: Box<dyn PhysicalOperator> = match project_wrap {
            Some((exprs, aliases)) => {
                let materialised: Vec<LogicalExpr> = exprs
                    .iter()
                    .map(|e| self.materialize_subqueries(e))
                    .collect::<Result<Vec<_>>>()?;
                Box::new(
                    ProjectOperator::new(scan, materialised, aliases.to_vec(), false, self.parameters.clone())
                        .with_timeout(self.timeout_ctx.clone()),
                )
            }
            None => scan,
        };

        // VectorScanOperator already returns at most k rows ordered by
        // ascending distance; the LimitOperator applies the offset skip and
        // final limit.
        Ok(Some(Box::new(
            LimitOperator::new(after_project, limit, offset).with_timeout(self.timeout_ctx.clone()),
        )))
    }

    /// Resolve an expression that should be a constant query vector (a vector
    /// literal, a `[...]`-string literal, or a bound `$N` parameter) into a
    /// concrete `Vec<f32>`. Returns `Ok(None)` for anything row-dependent or
    /// not vector-shaped so the caller can fall back to the scan path.
    fn resolve_const_query_vector(&self, expr: &crate::sql::LogicalExpr) -> Result<Option<Vec<f32>>> {
        use crate::sql::LogicalExpr;
        let value = match expr {
            LogicalExpr::Literal(v) => v.clone(),
            LogicalExpr::Parameter { index } => match self.parameters.get(index.saturating_sub(1)) {
                Some(v) => v.clone(),
                None => return Ok(None),
            },
            // `$1::vector` / `'[...]'::vector` — unwrap the cast and resolve
            // the inner constant; the value is already vector-shaped.
            LogicalExpr::Cast { expr, .. } => return self.resolve_const_query_vector(expr),
            _ => return Ok(None),
        };
        Ok(value_to_query_vector(&value))
    }

    fn direct_topk_project_spec(
        &self,
        input: &LogicalPlan,
        sort_exprs: &[crate::sql::LogicalExpr],
    ) -> Result<Option<DirectTopKProjectSpec>> {
        match input {
            LogicalPlan::Project {
                input,
                exprs,
                aliases,
                distinct: false,
                distinct_on: None,
            } => {
                let Some((table_name, scan_schema)) = self.direct_topk_scan_schema(input)? else {
                    return Ok(None);
                };
                let mut output_columns = Vec::with_capacity(exprs.len());
                for expr in exprs {
                    let Some(idx) = direct_expr_column_index(&scan_schema, expr) else {
                        return Ok(None);
                    };
                    output_columns.push(idx);
                }
                let output_schema = Arc::new(Schema {
                    columns: output_columns
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, &base_idx)| {
                            scan_schema.columns.get(base_idx).map(|base| {
                                let mut column = base.clone();
                                if let Some(alias) = aliases.get(idx).filter(|alias| !alias.is_empty()) {
                                    column.name = alias.clone();
                                }
                                column.source_table = None;
                                column.source_table_name = None;
                                column.primary_key = false;
                                column.unique = false;
                                column
                            })
                        })
                        .collect(),
                });
                if output_schema.columns.len() != output_columns.len() {
                    return Ok(None);
                }
                let Some(sort_columns) = resolve_sort_columns_to_base(sort_exprs, &output_schema, &output_columns)
                else {
                    return Ok(None);
                };
                Ok(Some(DirectTopKProjectSpec {
                    table_name,
                    scan_schema,
                    output_schema,
                    output_columns,
                    sort_columns,
                }))
            }
            LogicalPlan::Scan { projection, .. } => {
                let Some((table_name, scan_schema)) = self.direct_topk_scan_schema(input)? else {
                    return Ok(None);
                };
                let output_columns: Vec<usize> = projection
                    .clone()
                    .unwrap_or_else(|| (0..scan_schema.columns.len()).collect());
                let output_schema = Arc::new(Schema {
                    columns: output_columns
                        .iter()
                        .filter_map(|&idx| scan_schema.columns.get(idx).cloned())
                        .collect(),
                });
                if output_schema.columns.len() != output_columns.len() {
                    return Ok(None);
                }
                let Some(sort_columns) = resolve_sort_columns_to_base(sort_exprs, &output_schema, &output_columns)
                else {
                    return Ok(None);
                };
                Ok(Some(DirectTopKProjectSpec {
                    table_name,
                    scan_schema,
                    output_schema,
                    output_columns,
                    sort_columns,
                }))
            }
            _ => Ok(None),
        }
    }

    fn direct_topk_scan_schema(&self, input: &LogicalPlan) -> Result<Option<(String, Arc<Schema>)>> {
        let Some(storage) = self.storage else {
            return Ok(None);
        };
        let LogicalPlan::Scan {
            table_name,
            alias,
            schema,
            as_of,
            ..
        } = input
        else {
            return Ok(None);
        };
        if as_of.is_some()
            || self.get_cte(table_name).is_some()
            || storage.mv_catalog().view_exists(table_name)?
            || !storage.catalog().table_exists(table_name)?
        {
            return Ok(None);
        }
        let source_name = alias.as_ref().unwrap_or(table_name);
        let mut scan_schema = schema.as_ref().clone();
        for column in &mut scan_schema.columns {
            column.source_table = Some(source_name.clone());
            column.source_table_name = Some(table_name.clone());
        }
        Ok(Some((table_name.clone(), Arc::new(scan_schema))))
    }

    /// R5.V4: resolve the plan under a kNN Sort to a base-table scan plus an
    /// optional row-local WHERE predicate for post-filtering ANN candidates.
    ///
    /// Accepted shapes (all reading a single real table):
    ///   * `Scan`                 — unfiltered kNN (pre-V4 behaviour)
    ///   * `Filter(Scan)`         — WHERE the optimizer left at executor level
    ///   * `FilteredScan`         — WHERE pushed to storage
    ///   * `Filter(FilteredScan)` — pushable + residual conjuncts split
    ///
    /// Any predicate must pass [`Self::is_simple_knn_filter`] so that
    /// post-applying it to candidate tuples is guaranteed equivalent to the
    /// brute-force scan path. Returns `None` for every other shape so the
    /// caller falls back to scan+sort.
    fn knn_scan_with_filter(
        &self,
        input: &LogicalPlan,
    ) -> Result<Option<(String, Arc<Schema>, Option<crate::sql::LogicalExpr>)>> {
        use crate::sql::{BinaryOperator, LogicalExpr};

        // Peel one optional executor-level Filter off the top.
        let (residual, scan_plan): (Option<&LogicalExpr>, &LogicalPlan) = match input {
            LogicalPlan::Filter {
                input: inner,
                predicate,
            } => (Some(predicate), inner.as_ref()),
            other => (None, other),
        };

        let (table_name, scan_schema, pushed): (String, Arc<Schema>, Option<&LogicalExpr>) = match scan_plan {
            LogicalPlan::Scan { .. } => match self.direct_topk_scan_schema(scan_plan)? {
                Some((table_name, scan_schema)) => (table_name, scan_schema, None),
                None => return Ok(None),
            },
            LogicalPlan::FilteredScan {
                table_name,
                alias,
                schema,
                predicate,
                as_of,
                ..
            } => {
                // Mirrors the `direct_topk_scan_schema` eligibility checks
                // for the storage-pushdown scan variant.
                let Some(storage) = self.storage else {
                    return Ok(None);
                };
                if as_of.is_some()
                    || self.get_cte(table_name).is_some()
                    || storage.mv_catalog().view_exists(table_name)?
                    || !storage.catalog().table_exists(table_name)?
                {
                    return Ok(None);
                }
                let source_name = alias.as_ref().unwrap_or(table_name);
                let mut scan_schema = schema.as_ref().clone();
                for column in &mut scan_schema.columns {
                    column.source_table = Some(source_name.clone());
                    column.source_table_name = Some(table_name.clone());
                }
                (table_name.clone(), Arc::new(scan_schema), predicate.as_ref())
            }
            _ => return Ok(None),
        };

        // Recombine the pushed-down and residual conjuncts; the fast path
        // evaluates the whole predicate itself against candidate tuples.
        let combined: Option<LogicalExpr> = match (pushed, residual) {
            (None, None) => None,
            (Some(p), None) => Some(p.clone()),
            (None, Some(r)) => Some(r.clone()),
            (Some(p), Some(r)) => Some(LogicalExpr::BinaryExpr {
                left: Box::new(p.clone()),
                op: BinaryOperator::And,
                right: Box::new(r.clone()),
            }),
        };
        if let Some(pred) = &combined {
            if !Self::is_simple_knn_filter(pred, scan_schema.as_ref()) {
                return Ok(None);
            }
        }
        Ok(Some((table_name, scan_schema, combined)))
    }

    /// A predicate the kNN fast path may post-apply to candidate tuples:
    /// `column <cmp> constant` (either operand order; constant = literal or
    /// bound parameter) where the column resolves in the scan schema, or a
    /// conjunction (`AND`) of such comparisons. Everything else — OR, NOT,
    /// LIKE, functions, subqueries, column-vs-column — sends the query back
    /// to the brute-force path.
    fn is_simple_knn_filter(expr: &crate::sql::LogicalExpr, schema: &Schema) -> bool {
        use crate::sql::{BinaryOperator, LogicalExpr};

        let is_const = |e: &LogicalExpr| matches!(e, LogicalExpr::Literal(_) | LogicalExpr::Parameter { .. });
        let is_column = |e: &LogicalExpr| {
            if let LogicalExpr::Column { table, name } = e {
                schema.get_qualified_column_index(table.as_deref(), name).is_some()
            } else {
                false
            }
        };
        match expr {
            LogicalExpr::BinaryExpr { left, op, right } => match op {
                BinaryOperator::And => {
                    Self::is_simple_knn_filter(left, schema) && Self::is_simple_knn_filter(right, schema)
                }
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq => {
                    (is_column(left) && is_const(right)) || (is_const(left) && is_column(right))
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Materialize IN subqueries by executing them and converting to InList
    ///
    /// This allows the evaluator to handle IN expressions without needing
    /// access to the storage engine.
    pub(crate) fn materialize_subqueries(&self, expr: &crate::sql::LogicalExpr) -> Result<crate::sql::LogicalExpr> {
        use crate::sql::LogicalExpr;

        match expr {
            LogicalExpr::InSubquery {
                expr: inner_expr,
                subquery,
                negated,
            } => {
                // Execute the subquery to get the list of values
                let mut subquery_executor = if let Some(storage) = self.storage {
                    Executor::with_storage(storage)
                } else {
                    Executor::new()
                }
                .with_parameters(self.parameters.clone());

                let results = subquery_executor.execute(subquery)?;

                // Materialize the inner expression as well
                let materialized_inner = self.materialize_subqueries(inner_expr)?;

                // Use HashSet for large IN lists (O(1) lookup instead of O(N) linear scan)
                if results.len() > 16 {
                    let value_set: std::collections::HashSet<crate::Value> = results
                        .iter()
                        .filter_map(|tuple| tuple.values.first().cloned())
                        .collect();
                    Ok(LogicalExpr::InSet {
                        expr: Box::new(materialized_inner),
                        values: value_set,
                        negated: *negated,
                    })
                } else {
                    let list: Vec<LogicalExpr> = results
                        .iter()
                        .filter_map(|tuple| tuple.values.first().map(|v| LogicalExpr::Literal(v.clone())))
                        .collect();
                    Ok(LogicalExpr::InList {
                        expr: Box::new(materialized_inner),
                        list,
                        negated: *negated,
                    })
                }
            }
            LogicalExpr::ScalarSubquery { subquery } => {
                // Execute the subquery once. A scalar subquery returns
                // the first column of the first row (or NULL if the
                // query returns zero rows). This branch runs at plan
                // build time, so it only handles UNCORRELATED scalar
                // subqueries — the UPDATE executor calls
                // `materialize_scalar_subquery_with_outer` before
                // per-row evaluation when correlation is involved.
                let mut subquery_executor = if let Some(storage) = self.storage {
                    Executor::with_storage(storage)
                } else {
                    Executor::new()
                }
                .with_parameters(self.parameters.clone());

                // KanttBan #23 phase 2.10: same fallback as
                // correlated EXISTS (phase 2.5b) — when the inner
                // SELECT references an outer column we can't resolve
                // (e.g. `(SELECT oid FROM pg_class WHERE relname =
                // tc.table_name)` in drizzle's info_schema query),
                // swallow the error and return NULL. Genuine
                // correlated-subquery support needs nested-loop or
                // dependent rewrite; this lets drizzle keep going
                // (the JOIN evaluates with the NULL on one side →
                // ON-clause is false → no match).
                let results = match subquery_executor.execute(subquery) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!("Correlated scalar subquery failed ({e}); falling back to NULL");
                        Vec::new()
                    }
                };
                let value = results
                    .first()
                    .and_then(|tuple| tuple.values.first().cloned())
                    .unwrap_or(crate::Value::Null);
                Ok(LogicalExpr::Literal(value))
            }
            LogicalExpr::Exists { subquery, negated } => {
                // Execute the subquery to check if any rows exist
                let mut subquery_executor = if let Some(storage) = self.storage {
                    Executor::with_storage(storage)
                } else {
                    Executor::new()
                }
                .with_parameters(self.parameters.clone());

                // KanttBan #23 phase 2.5: correlated EXISTS
                // (inner WHERE references outer columns) fails here
                // with "Column 'a.attrelid' not found in schema"
                // because we materialise the subquery once with no
                // outer-row context. drizzle's getColumnsInfoQuery
                // uses correlated EXISTS for SERIAL detection — true
                // correlated-subquery support needs nested-loop join
                // or dependent-rewrite (significant planner work,
                // deferred). For now: swallow the error and treat
                // EXISTS as false. drizzle's CASE then falls through
                // to format_type, which is what we want. Other paths
                // that need accurate correlated EXISTS will need to
                // wait for the full implementation.
                let results = match subquery_executor.execute(subquery) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!(
                            "Correlated EXISTS subquery failed ({e}); falling back to false. \
                             True correlated-subquery support is tracked as future work."
                        );
                        Vec::new()
                    }
                };

                // EXISTS returns true if subquery returns any rows
                let exists = !results.is_empty();
                let result = if *negated { !exists } else { exists };

                Ok(LogicalExpr::Literal(crate::Value::Boolean(result)))
            }
            // Recursively process compound expressions
            LogicalExpr::BinaryExpr { left, op, right } => Ok(LogicalExpr::BinaryExpr {
                left: Box::new(self.materialize_subqueries(left)?),
                op: *op,
                right: Box::new(self.materialize_subqueries(right)?),
            }),
            LogicalExpr::UnaryExpr { op, expr: inner } => Ok(LogicalExpr::UnaryExpr {
                op: *op,
                expr: Box::new(self.materialize_subqueries(inner)?),
            }),
            LogicalExpr::IsNull { expr: inner, is_null } => Ok(LogicalExpr::IsNull {
                expr: Box::new(self.materialize_subqueries(inner)?),
                is_null: *is_null,
            }),
            LogicalExpr::Between {
                expr: inner,
                low,
                high,
                negated,
            } => Ok(LogicalExpr::Between {
                expr: Box::new(self.materialize_subqueries(inner)?),
                low: Box::new(self.materialize_subqueries(low)?),
                high: Box::new(self.materialize_subqueries(high)?),
                negated: *negated,
            }),
            LogicalExpr::InList {
                expr: inner,
                list,
                negated,
            } => {
                let materialized_list: Result<Vec<LogicalExpr>> =
                    list.iter().map(|e| self.materialize_subqueries(e)).collect();
                Ok(LogicalExpr::InList {
                    expr: Box::new(self.materialize_subqueries(inner)?),
                    list: materialized_list?,
                    negated: *negated,
                })
            }
            LogicalExpr::Case {
                expr: operand,
                when_then,
                else_result,
            } => {
                let materialized_operand = if let Some(op) = operand {
                    Some(Box::new(self.materialize_subqueries(op)?))
                } else {
                    None
                };
                let materialized_when_then: Result<Vec<(LogicalExpr, LogicalExpr)>> = when_then
                    .iter()
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

    // ============================ Correlated subqueries ============================
    // `materialize_subqueries` runs once at plan-build time with no outer row, so a
    // CORRELATED subquery (one referencing an outer column) errors and is swallowed
    // to false/NULL. The helpers below evaluate correlated subqueries per outer row:
    // each subquery's inner plan has its FREE (outer-referencing) column refs bound
    // to the current outer row's values, then it is executed. See the Filter arm.

    /// Base-table schema of a (single-table) subquery plan, used to decide whether a
    /// column reference is satisfied by the subquery's own table (inner) or is FREE
    /// (a correlated outer reference).
    fn base_scan_schema(plan: &LogicalPlan) -> Option<std::sync::Arc<Schema>> {
        match plan {
            LogicalPlan::Scan { schema, .. } | LogicalPlan::FilteredScan { schema, .. } => Some(schema.clone()),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. } => Self::base_scan_schema(input),
            _ => None,
        }
    }

    /// A column ref is a FREE outer reference if the subquery's own base table does
    /// not provide it but `outer` does.
    fn col_is_free_outer(table: &Option<String>, name: &str, inner: Option<&Schema>, outer: &Schema) -> bool {
        let in_inner = inner.is_some_and(|s| s.get_qualified_column_index(table.as_deref(), name).is_some());
        !in_inner && outer.get_qualified_column_index(table.as_deref(), name).is_some()
    }

    fn expr_has_free_outer_ref(expr: &crate::sql::LogicalExpr, inner: Option<&Schema>, outer: &Schema) -> bool {
        use crate::sql::LogicalExpr as E;
        match expr {
            E::Column { table, name } => Self::col_is_free_outer(table, name, inner, outer),
            E::BinaryExpr { left, right, .. } => {
                Self::expr_has_free_outer_ref(left, inner, outer) || Self::expr_has_free_outer_ref(right, inner, outer)
            }
            E::UnaryExpr { expr, .. } | E::IsNull { expr, .. } => Self::expr_has_free_outer_ref(expr, inner, outer),
            E::Between { expr, low, high, .. } => {
                Self::expr_has_free_outer_ref(expr, inner, outer)
                    || Self::expr_has_free_outer_ref(low, inner, outer)
                    || Self::expr_has_free_outer_ref(high, inner, outer)
            }
            E::InList { expr, list, .. } => {
                Self::expr_has_free_outer_ref(expr, inner, outer)
                    || list.iter().any(|e| Self::expr_has_free_outer_ref(e, inner, outer))
            }
            E::Case {
                expr,
                when_then,
                else_result,
            } => {
                expr.as_ref()
                    .is_some_and(|e| Self::expr_has_free_outer_ref(e, inner, outer))
                    || when_then.iter().any(|(w, t)| {
                        Self::expr_has_free_outer_ref(w, inner, outer) || Self::expr_has_free_outer_ref(t, inner, outer)
                    })
                    || else_result
                        .as_ref()
                        .is_some_and(|e| Self::expr_has_free_outer_ref(e, inner, outer))
            }
            _ => false,
        }
    }

    fn plan_has_free_outer_ref(plan: &LogicalPlan, outer: &Schema) -> bool {
        match plan {
            LogicalPlan::FilteredScan {
                schema,
                predicate: Some(p),
                ..
            } => Self::expr_has_free_outer_ref(p, Some(schema), outer),
            LogicalPlan::Filter { input, predicate } => {
                let inner = Self::base_scan_schema(input);
                Self::expr_has_free_outer_ref(predicate, inner.as_deref(), outer)
                    || Self::plan_has_free_outer_ref(input, outer)
            }
            LogicalPlan::Project { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. } => Self::plan_has_free_outer_ref(input, outer),
            _ => false,
        }
    }

    /// True if `expr` contains a correlated subquery (vs the predicate's `outer` schema).
    fn expr_has_correlated_subquery(&self, expr: &crate::sql::LogicalExpr, outer: &Schema) -> bool {
        use crate::sql::LogicalExpr as E;
        match expr {
            E::Exists { subquery, .. } | E::ScalarSubquery { subquery } | E::InSubquery { subquery, .. } => {
                Self::plan_has_free_outer_ref(subquery, outer)
            }
            E::BinaryExpr { left, right, .. } => {
                self.expr_has_correlated_subquery(left, outer) || self.expr_has_correlated_subquery(right, outer)
            }
            E::UnaryExpr { expr, .. } | E::IsNull { expr, .. } => self.expr_has_correlated_subquery(expr, outer),
            E::Between { expr, low, high, .. } => {
                self.expr_has_correlated_subquery(expr, outer)
                    || self.expr_has_correlated_subquery(low, outer)
                    || self.expr_has_correlated_subquery(high, outer)
            }
            E::InList { expr, list, .. } => {
                self.expr_has_correlated_subquery(expr, outer)
                    || list.iter().any(|e| self.expr_has_correlated_subquery(e, outer))
            }
            E::Case {
                expr,
                when_then,
                else_result,
            } => {
                expr.as_ref()
                    .is_some_and(|e| self.expr_has_correlated_subquery(e, outer))
                    || when_then.iter().any(|(w, t)| {
                        self.expr_has_correlated_subquery(w, outer) || self.expr_has_correlated_subquery(t, outer)
                    })
                    || else_result
                        .as_ref()
                        .is_some_and(|e| self.expr_has_correlated_subquery(e, outer))
            }
            _ => false,
        }
    }

    /// Bind a subquery's FREE outer column refs to the outer row's values (in place).
    fn bind_expr_to_outer(expr: &mut crate::sql::LogicalExpr, inner: Option<&Schema>, outer: &Schema, row: &Tuple) {
        use crate::sql::LogicalExpr as E;
        match expr {
            E::Column { table, name } => {
                if inner.is_some_and(|s| s.get_qualified_column_index(table.as_deref(), name).is_some()) {
                    return; // inner column — leave for the subquery to resolve
                }
                if let Some(idx) = outer.get_qualified_column_index(table.as_deref(), name) {
                    if let Some(v) = row.values.get(idx) {
                        *expr = E::Literal(v.clone());
                    }
                }
            }
            E::BinaryExpr { left, right, .. } => {
                Self::bind_expr_to_outer(left, inner, outer, row);
                Self::bind_expr_to_outer(right, inner, outer, row);
            }
            E::UnaryExpr { expr, .. } | E::IsNull { expr, .. } => Self::bind_expr_to_outer(expr, inner, outer, row),
            E::Between { expr, low, high, .. } => {
                Self::bind_expr_to_outer(expr, inner, outer, row);
                Self::bind_expr_to_outer(low, inner, outer, row);
                Self::bind_expr_to_outer(high, inner, outer, row);
            }
            E::InList { expr, list, .. } => {
                Self::bind_expr_to_outer(expr, inner, outer, row);
                for e in list {
                    Self::bind_expr_to_outer(e, inner, outer, row);
                }
            }
            E::Case {
                expr,
                when_then,
                else_result,
            } => {
                if let Some(e) = expr {
                    Self::bind_expr_to_outer(e, inner, outer, row);
                }
                for (w, t) in when_then {
                    Self::bind_expr_to_outer(w, inner, outer, row);
                    Self::bind_expr_to_outer(t, inner, outer, row);
                }
                if let Some(e) = else_result {
                    Self::bind_expr_to_outer(e, inner, outer, row);
                }
            }
            _ => {}
        }
    }

    /// Return a clone of `plan` with its predicates' free outer column refs bound to
    /// the outer row (so a correlated subquery executes for that specific row).
    fn bind_plan_to_outer(plan: &LogicalPlan, outer: &Schema, row: &Tuple) -> LogicalPlan {
        let mut p = plan.clone();
        Self::bind_plan_mut(&mut p, outer, row);
        p
    }

    fn bind_plan_mut(plan: &mut LogicalPlan, outer: &Schema, row: &Tuple) {
        match plan {
            LogicalPlan::FilteredScan { schema, predicate, .. } => {
                let inner = schema.clone();
                if let Some(p) = predicate {
                    Self::bind_expr_to_outer(p, Some(&inner), outer, row);
                }
            }
            LogicalPlan::Filter { input, predicate } => {
                let inner = Self::base_scan_schema(input);
                Self::bind_expr_to_outer(predicate, inner.as_deref(), outer, row);
                Self::bind_plan_mut(input, outer, row);
            }
            LogicalPlan::Project { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. } => Self::bind_plan_mut(input, outer, row),
            _ => {}
        }
    }

    /// Like `materialize_subqueries`, but evaluates each subquery for a specific
    /// OUTER row (binding free outer refs first), so correlated subqueries are
    /// correct. On execution failure it falls back to the same false/NULL the
    /// uncorrelated path uses (preserves drizzle / info_schema introspection).
    fn materialize_subqueries_with_outer(
        &self,
        expr: &crate::sql::LogicalExpr,
        outer: &Schema,
        row: &Tuple,
    ) -> Result<crate::sql::LogicalExpr> {
        use crate::sql::LogicalExpr as E;
        let run = |plan: &LogicalPlan| -> Vec<Tuple> {
            let bound = Self::bind_plan_to_outer(plan, outer, row);
            let mut ex = if let Some(s) = self.storage {
                Executor::with_storage(s)
            } else {
                Executor::new()
            }
            .with_parameters(self.parameters.clone());
            // Carry materialized CTEs so a correlated subquery can reference a
            // WITH relation defined in the outer query (e.g. EXISTS over a CTE).
            ex.cte_context = self.cte_context.clone();
            ex.execute(&bound).unwrap_or_default()
        };
        match expr {
            E::InSubquery {
                expr: inner_expr,
                subquery,
                negated,
            } => {
                let results = run(subquery);
                let materialized_inner = self.materialize_subqueries_with_outer(inner_expr, outer, row)?;
                let list: Vec<E> = results
                    .iter()
                    .filter_map(|t| t.values.first().map(|v| E::Literal(v.clone())))
                    .collect();
                Ok(E::InList {
                    expr: Box::new(materialized_inner),
                    list,
                    negated: *negated,
                })
            }
            E::ScalarSubquery { subquery } => {
                let results = run(subquery);
                let value = results
                    .first()
                    .and_then(|t| t.values.first().cloned())
                    .unwrap_or(crate::Value::Null);
                Ok(E::Literal(value))
            }
            E::Exists { subquery, negated } => {
                let exists = !run(subquery).is_empty();
                Ok(E::Literal(crate::Value::Boolean(if *negated {
                    !exists
                } else {
                    exists
                })))
            }
            E::BinaryExpr { left, op, right } => Ok(E::BinaryExpr {
                left: Box::new(self.materialize_subqueries_with_outer(left, outer, row)?),
                op: *op,
                right: Box::new(self.materialize_subqueries_with_outer(right, outer, row)?),
            }),
            E::UnaryExpr { op, expr: inner } => Ok(E::UnaryExpr {
                op: *op,
                expr: Box::new(self.materialize_subqueries_with_outer(inner, outer, row)?),
            }),
            E::IsNull { expr: inner, is_null } => Ok(E::IsNull {
                expr: Box::new(self.materialize_subqueries_with_outer(inner, outer, row)?),
                is_null: *is_null,
            }),
            E::Between {
                expr: inner,
                low,
                high,
                negated,
            } => Ok(E::Between {
                expr: Box::new(self.materialize_subqueries_with_outer(inner, outer, row)?),
                low: Box::new(self.materialize_subqueries_with_outer(low, outer, row)?),
                high: Box::new(self.materialize_subqueries_with_outer(high, outer, row)?),
                negated: *negated,
            }),
            E::InList {
                expr: inner,
                list,
                negated,
            } => {
                let ml: Result<Vec<E>> = list
                    .iter()
                    .map(|e| self.materialize_subqueries_with_outer(e, outer, row))
                    .collect();
                Ok(E::InList {
                    expr: Box::new(self.materialize_subqueries_with_outer(inner, outer, row)?),
                    list: ml?,
                    negated: *negated,
                })
            }
            E::Case {
                expr: operand,
                when_then,
                else_result,
            } => {
                let mo = match operand {
                    Some(op) => Some(Box::new(self.materialize_subqueries_with_outer(op, outer, row)?)),
                    None => None,
                };
                let mwt: Result<Vec<(E, E)>> = when_then
                    .iter()
                    .map(|(w, t)| {
                        Ok((
                            self.materialize_subqueries_with_outer(w, outer, row)?,
                            self.materialize_subqueries_with_outer(t, outer, row)?,
                        ))
                    })
                    .collect();
                let me = match else_result {
                    Some(e) => Some(Box::new(self.materialize_subqueries_with_outer(e, outer, row)?)),
                    None => None,
                };
                Ok(E::Case {
                    expr: mo,
                    when_then: mwt?,
                    else_result: me,
                })
            }
            _ => Ok(expr.clone()),
        }
    }

    fn count_star_schema_operator(count: i64) -> Box<dyn PhysicalOperator> {
        Box::new(MaterializedOperator::new(
            vec![crate::Tuple::new(vec![crate::Value::Int8(count)])],
            count_star_schema(),
        ))
    }

    fn fast_path_storage_table_name(&self, table_name: &str) -> Result<String> {
        let Some(storage) = self.storage else {
            return Ok(table_name.to_string());
        };

        let mv_catalog = storage.mv_catalog();
        if !mv_catalog.view_exists(table_name)? {
            return Ok(table_name.to_string());
        }

        let mv_data_table = crate::storage::MaterializedViewCatalog::mv_data_table_name(table_name);
        if !storage.catalog().table_exists(&mv_data_table)? {
            return Err(Error::query_execution(format!(
                "Materialized view '{}' exists but has never been refreshed. Run: REFRESH MATERIALIZED VIEW {}",
                table_name, table_name
            )));
        }

        Ok(mv_data_table)
    }

    fn count_distinct_schema_operator(
        count: i64,
        group_by: &[crate::sql::LogicalExpr],
        aggr_exprs: &[crate::sql::LogicalExpr],
        input_schema: &Schema,
    ) -> Box<dyn PhysicalOperator> {
        Box::new(MaterializedOperator::new(
            vec![crate::Tuple::new(vec![crate::Value::Int8(count)])],
            AggregateOperator::output_schema(group_by, aggr_exprs, input_schema),
        ))
    }

    fn try_count_pk_cardinality(
        &mut self,
        input: &LogicalPlan,
        group_by: &[crate::sql::LogicalExpr],
        aggr_exprs: &[crate::sql::LogicalExpr],
        having: &Option<crate::sql::LogicalExpr>,
    ) -> Result<Option<Box<dyn PhysicalOperator>>> {
        use crate::sql::logical_plan::AggregateFunction;
        use crate::sql::LogicalExpr;

        // R2.3: transaction check is per-table once the scan target is known.
        if !group_by.is_empty() || having.is_some() || aggr_exprs.len() != 1 || self.txn_forces_slow_reads() {
            return Ok(None);
        }
        let storage = match self.storage {
            Some(storage) => storage,
            None => return Ok(None),
        };
        if storage.is_branch_active() {
            return Ok(None);
        }

        let LogicalExpr::AggregateFunction {
            fun: AggregateFunction::Count,
            args,
            distinct: _,
        } = &aggr_exprs[0]
        else {
            return Ok(None);
        };
        let Some(arg) = args.first() else {
            return Ok(None);
        };
        if matches!(arg, LogicalExpr::Wildcard) {
            return Ok(None);
        }

        let Some((table_name, schema, predicate, as_of)) = Self::columnar_aggregate_input(input) else {
            return Ok(None);
        };
        if as_of.is_some() || self.get_cte(table_name).is_some() || self.txn_forces_slow_reads_for_table(table_name) {
            return Ok(None);
        }

        let Some(arg_idx) = Self::column_expr_index(arg, schema) else {
            return Ok(None);
        };
        let Some(pk_col) = schema.columns.get(arg_idx).filter(|col| col.primary_key) else {
            return Ok(None);
        };
        if schema.columns.iter().filter(|col| col.primary_key).count() != 1 {
            return Ok(None);
        }

        let count_table_name = self.fast_path_storage_table_name(table_name)?;
        // R2.3: `table_name` may be a materialized view resolved to its
        // backing data table — staged writes are attributed to the latter.
        if self.txn_forces_slow_reads_for_table(&count_table_name) {
            return Ok(None);
        }
        let count = match predicate {
            None => storage.count_table_rows(&count_table_name)?,
            Some(predicate) => match self.count_single_pk_predicate(table_name, schema, pk_col, predicate)? {
                Some(count) => count,
                None => return Ok(None),
            },
        };

        Ok(Some(Self::count_distinct_schema_operator(
            count as i64,
            group_by,
            aggr_exprs,
            schema,
        )))
    }

    fn count_single_pk_predicate(
        &mut self,
        table_name: &str,
        schema: &Schema,
        pk_col: &crate::Column,
        predicate: &crate::sql::LogicalExpr,
    ) -> Result<Option<usize>> {
        let storage = match self.storage {
            Some(storage) => storage,
            None => return Ok(None),
        };
        if storage.is_branch_active() {
            return Ok(None);
        }
        let predicate = self.materialize_subqueries(predicate)?;
        if let Some((lower, upper)) = self.pk_int_range_from_predicate(&predicate, &pk_col.name, &pk_col.data_type) {
            return storage.count_table_pk_int_range_with_schema(table_name, schema, lower, upper);
        }
        self.count_single_pk_in_list(table_name, pk_col, &predicate)
    }

    fn count_single_pk_in_list(
        &self,
        table_name: &str,
        pk_col: &crate::Column,
        predicate: &crate::sql::LogicalExpr,
    ) -> Result<Option<usize>> {
        use crate::sql::LogicalExpr;

        let LogicalExpr::InList {
            expr,
            list,
            negated: false,
        } = predicate
        else {
            return Ok(None);
        };
        if !Self::expr_matches_column(expr, &pk_col.name) {
            return Ok(None);
        }
        let storage = match self.storage {
            Some(storage) => storage,
            None => return Ok(None),
        };

        let mut seen = std::collections::HashSet::new();
        let mut count = 0usize;
        for item in list {
            let Some(value) = self.pk_in_list_value(item, &pk_col.data_type) else {
                return Ok(None);
            };
            if matches!(value, crate::Value::Null) {
                continue;
            }
            let key = crate::storage::ArtIndexManager::encode_key(std::slice::from_ref(&value));
            if seen.insert(key.clone()) {
                match storage.art_indexes().pk_index_contains(table_name, &key) {
                    Some(true) => count += 1,
                    Some(false) => {}
                    None => return Ok(None),
                }
            }
        }

        Ok(Some(count))
    }

    fn pk_in_list_value(&self, expr: &crate::sql::LogicalExpr, pk_type: &crate::DataType) -> Option<crate::Value> {
        use crate::sql::LogicalExpr;
        use crate::{DataType, Value};

        let value = match expr {
            LogicalExpr::Literal(value) => value.clone(),
            LogicalExpr::Parameter { index } => self.parameters.get(index.saturating_sub(1)).cloned()?,
            _ => return None,
        };

        if matches!(value, Value::Null) {
            return Some(Value::Null);
        }

        match pk_type {
            DataType::Int2 => {
                let raw = Self::value_to_i64_for_pk_range(&value, pk_type)?;
                i16::try_from(raw).ok().map(Value::Int2)
            }
            DataType::Int4 => {
                let raw = Self::value_to_i64_for_pk_range(&value, pk_type)?;
                i32::try_from(raw).ok().map(Value::Int4)
            }
            DataType::Int8 => {
                let raw = Self::value_to_i64_for_pk_range(&value, pk_type)?;
                Some(Value::Int8(raw))
            }
            _ => Some(self::coerce_literal_to_column_type(value, pk_type)),
        }
    }

    /// R2.3 item 2: apply HAVING as a post-filter over the (small) output of
    /// an aggregate pushdown, mirroring the slow path in
    /// `AggregateOperator::new` exactly: aggregate calls are rewritten to
    /// `agg_{i}` column references and groups whose predicate doesn't
    /// evaluate to `Boolean(true)` are dropped (including evaluation errors —
    /// identical semantics in and out of transactions).
    fn apply_having_post_filter(
        &mut self,
        tuples: Vec<crate::Tuple>,
        output_schema: &Arc<Schema>,
        having: &Option<crate::sql::LogicalExpr>,
        aggr_exprs: &[crate::sql::LogicalExpr],
    ) -> Result<Vec<crate::Tuple>> {
        let Some(having_expr) = having else {
            return Ok(tuples);
        };
        // Same pre-step the slow path performs before AggregateOperator::new:
        // (sub)queries in HAVING must be materialized or every group drops.
        let having_expr = self.materialize_subqueries(having_expr)?;
        let rewritten = AggregateOperator::rewrite_having_expr(&having_expr, aggr_exprs);
        let evaluator = crate::sql::Evaluator::new(output_schema.clone());
        Ok(tuples
            .into_iter()
            .filter(|tuple| matches!(evaluator.evaluate(&rewritten, tuple), Ok(crate::Value::Boolean(true))))
            .collect())
    }

    fn try_columnar_aggregate(
        &mut self,
        input: &LogicalPlan,
        group_by: &[crate::sql::LogicalExpr],
        aggr_exprs: &[crate::sql::LogicalExpr],
        having: &Option<crate::sql::LogicalExpr>,
    ) -> Result<Option<Box<dyn PhysicalOperator>>> {
        // R2.3: HAVING is handled as a post-filter; the transaction check is
        // per-table once the scan target is known.
        if self.txn_forces_slow_reads() {
            return Ok(None);
        }
        let storage = match self.storage {
            Some(storage) => storage,
            None => return Ok(None),
        };
        if storage.is_branch_active() {
            return Ok(None);
        }

        let Some((table_name, schema, predicate, as_of)) = Self::columnar_aggregate_input(input) else {
            return Ok(None);
        };
        if as_of.is_some() || self.get_cte(table_name).is_some() || self.txn_forces_slow_reads_for_table(table_name) {
            return Ok(None);
        }

        let predicate = predicate
            .map(|predicate| self.materialize_subqueries(predicate))
            .transpose()?;
        if predicate
            .as_ref()
            .is_some_and(|predicate| !Self::is_simple_columnar_pushdown_predicate(predicate))
        {
            return Ok(None);
        }
        let analyzed_predicates = predicate
            .as_ref()
            .map(|predicate| storage.predicate_pushdown().analyze_predicate(predicate, schema))
            .unwrap_or_default();
        if predicate.is_some() && analyzed_predicates.is_empty() {
            return Ok(None);
        }

        let mut group_indices = Vec::with_capacity(group_by.len());
        for expr in group_by {
            let Some(idx) = Self::column_expr_index(expr, schema) else {
                return Ok(None);
            };
            group_indices.push(idx);
        }

        let mut aggregate_specs = Vec::with_capacity(aggr_exprs.len());
        for expr in aggr_exprs {
            let Some(spec) = Self::columnar_aggregate_spec(expr, schema) else {
                return Ok(None);
            };
            aggregate_specs.push(spec);
        }

        let mut referenced = group_indices.clone();
        referenced.extend(aggregate_specs.iter().filter_map(|spec| spec.column_index));
        referenced.extend(analyzed_predicates.iter().map(|predicate| predicate.column_index));
        referenced.sort_unstable();
        referenced.dedup();
        if referenced.is_empty()
            || referenced.iter().any(|&idx| {
                schema
                    .columns
                    .get(idx)
                    .map_or(true, |column| column.storage_mode != crate::ColumnStorageMode::Columnar)
            })
        {
            return Ok(None);
        }

        let storage_table_name = self.fast_path_storage_table_name(table_name)?;
        // R2.3: `table_name` may be a materialized view resolved to its
        // backing data table — staged writes are attributed to the latter.
        if self.txn_forces_slow_reads_for_table(&storage_table_name) {
            return Ok(None);
        }
        let tuples = storage.aggregate_columnar_columns(
            &storage_table_name,
            schema,
            &group_indices,
            &aggregate_specs,
            &analyzed_predicates,
        )?;
        let output_schema = AggregateOperator::output_schema(group_by, aggr_exprs, schema);
        let tuples = self.apply_having_post_filter(tuples, &output_schema, having, aggr_exprs)?;
        Ok(Some(Box::new(MaterializedOperator::new(tuples, output_schema))))
    }

    fn try_rowstore_aggregate(
        &mut self,
        input: &LogicalPlan,
        group_by: &[crate::sql::LogicalExpr],
        aggr_exprs: &[crate::sql::LogicalExpr],
        having: &Option<crate::sql::LogicalExpr>,
    ) -> Result<Option<Box<dyn PhysicalOperator>>> {
        // R2.3: HAVING is handled as a post-filter; the transaction check is
        // per-table once the scan target is known.
        if self.txn_forces_slow_reads() {
            return Ok(None);
        }
        let storage = match self.storage {
            Some(storage) => storage,
            None => return Ok(None),
        };
        if storage.is_branch_active() {
            return Ok(None);
        }

        let Some((table_name, schema, predicate, as_of)) = Self::columnar_aggregate_input(input) else {
            return Ok(None);
        };
        if as_of.is_some() || self.get_cte(table_name).is_some() || self.txn_forces_slow_reads_for_table(table_name) {
            return Ok(None);
        }

        let predicate = predicate
            .map(|predicate| self.materialize_subqueries(predicate))
            .transpose()?;
        if predicate
            .as_ref()
            .is_some_and(|predicate| !Self::is_simple_columnar_pushdown_predicate(predicate))
        {
            return Ok(None);
        }
        let analyzed_predicates = predicate
            .as_ref()
            .map(|predicate| storage.predicate_pushdown().analyze_predicate(predicate, schema))
            .unwrap_or_default();
        if predicate.is_some() && analyzed_predicates.is_empty() {
            return Ok(None);
        }
        if !Self::rowstore_aggregate_predicates_are_sql_safe(schema, &analyzed_predicates) {
            return Ok(None);
        }

        let mut group_indices = Vec::with_capacity(group_by.len());
        for expr in group_by {
            let Some(idx) = Self::column_expr_index(expr, schema) else {
                return Ok(None);
            };
            group_indices.push(idx);
        }

        let mut aggregate_specs = Vec::with_capacity(aggr_exprs.len());
        for expr in aggr_exprs {
            let Some(spec) = Self::columnar_aggregate_spec(expr, schema) else {
                return Ok(None);
            };
            aggregate_specs.push(spec);
        }

        let mut referenced = group_indices.clone();
        referenced.extend(aggregate_specs.iter().filter_map(|spec| spec.column_index));
        referenced.extend(analyzed_predicates.iter().map(|predicate| predicate.column_index));
        referenced.sort_unstable();
        referenced.dedup();
        if referenced.iter().any(|&idx| {
            schema
                .columns
                .get(idx)
                .map_or(true, |column| column.storage_mode != crate::ColumnStorageMode::Default)
        }) {
            return Ok(None);
        }

        let storage_table_name = self.fast_path_storage_table_name(table_name)?;
        // R2.3: `table_name` may be a materialized view resolved to its
        // backing data table — staged writes are attributed to the latter.
        if self.txn_forces_slow_reads_for_table(&storage_table_name) {
            return Ok(None);
        }
        let Some(tuples) = storage.try_aggregate_row_columns(
            &storage_table_name,
            schema,
            &group_indices,
            &aggregate_specs,
            &analyzed_predicates,
        )?
        else {
            return Ok(None);
        };
        let output_schema = AggregateOperator::output_schema(group_by, aggr_exprs, schema);
        let tuples = self.apply_having_post_filter(tuples, &output_schema, having, aggr_exprs)?;
        Ok(Some(Box::new(MaterializedOperator::new(tuples, output_schema))))
    }

    fn rowstore_aggregate_predicates_are_sql_safe(
        schema: &Schema,
        predicates: &[crate::storage::predicate_pushdown::AnalyzedPredicate],
    ) -> bool {
        use crate::storage::predicate_pushdown::PredicateOp;

        if !scan::storage_predicates_are_sql_safe(schema, predicates) {
            return false;
        }

        predicates.iter().all(|predicate| match predicate.op {
            PredicateOp::Eq
            | PredicateOp::Lt
            | PredicateOp::LtEq
            | PredicateOp::Gt
            | PredicateOp::GtEq
            | PredicateOp::Like => !matches!(predicate.value, crate::Value::Null),
            PredicateOp::Between => {
                !matches!(predicate.value, crate::Value::Null)
                    && !matches!(predicate.value2, None | Some(crate::Value::Null))
            }
            PredicateOp::In => predicate
                .value_list
                .iter()
                .all(|value| !matches!(value, crate::Value::Null)),
            PredicateOp::IsNull | PredicateOp::IsNotNull => true,
            // FilterPredicate::NotEq/NotIn treat NULL as a positive match; keep
            // SQL three-valued logic on the generic evaluator path.
            PredicateOp::NotEq => false,
        })
    }

    fn columnar_aggregate_input<'b>(
        input: &'b LogicalPlan,
    ) -> Option<(
        &'b str,
        &'b Schema,
        Option<&'b crate::sql::LogicalExpr>,
        Option<&'b crate::sql::logical_plan::AsOfClause>,
    )> {
        match input {
            LogicalPlan::Scan {
                table_name,
                schema,
                projection,
                as_of,
                ..
            } if projection.is_none() => Some((table_name.as_str(), schema.as_ref(), None, as_of.as_ref())),
            LogicalPlan::FilteredScan {
                table_name,
                schema,
                projection,
                predicate,
                as_of,
                ..
            } if projection.is_none() => {
                Some((table_name.as_str(), schema.as_ref(), predicate.as_ref(), as_of.as_ref()))
            }
            LogicalPlan::Filter { input, predicate } => match input.as_ref() {
                LogicalPlan::Scan {
                    table_name,
                    schema,
                    projection,
                    as_of,
                    ..
                } if projection.is_none() => {
                    Some((table_name.as_str(), schema.as_ref(), Some(predicate), as_of.as_ref()))
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn column_expr_index(expr: &crate::sql::LogicalExpr, schema: &Schema) -> Option<usize> {
        match expr {
            crate::sql::LogicalExpr::Column { table, name } => schema
                .get_qualified_column_index(table.as_deref(), name)
                .or_else(|| schema.get_column_index(name)),
            _ => None,
        }
    }

    fn columnar_aggregate_spec(
        expr: &crate::sql::LogicalExpr,
        schema: &Schema,
    ) -> Option<crate::storage::ColumnarAggregateSpec> {
        use crate::sql::logical_plan::AggregateFunction;
        use crate::sql::LogicalExpr;
        use crate::storage::{ColumnarAggregateOp, ColumnarAggregateSpec};

        let LogicalExpr::AggregateFunction { fun, args, distinct } = expr else {
            return None;
        };
        let arg = args.first()?;
        match fun {
            AggregateFunction::Count if !distinct && matches!(arg, LogicalExpr::Wildcard) => {
                Some(ColumnarAggregateSpec {
                    op: ColumnarAggregateOp::CountStar,
                    column_index: None,
                })
            }
            AggregateFunction::Count if *distinct => {
                if matches!(arg, LogicalExpr::Wildcard) {
                    return None;
                }
                Some(ColumnarAggregateSpec {
                    op: ColumnarAggregateOp::CountDistinct,
                    column_index: Some(Self::column_expr_index(arg, schema)?),
                })
            }
            AggregateFunction::Count => Some(ColumnarAggregateSpec {
                op: ColumnarAggregateOp::Count,
                column_index: Some(Self::column_expr_index(arg, schema)?),
            }),
            AggregateFunction::Sum if !distinct => Some(ColumnarAggregateSpec {
                op: ColumnarAggregateOp::Sum,
                column_index: Some(Self::column_expr_index(arg, schema)?),
            }),
            AggregateFunction::Avg if !distinct => Some(ColumnarAggregateSpec {
                op: ColumnarAggregateOp::Avg,
                column_index: Some(Self::column_expr_index(arg, schema)?),
            }),
            AggregateFunction::Min if !distinct => Some(ColumnarAggregateSpec {
                op: ColumnarAggregateOp::Min,
                column_index: Some(Self::column_expr_index(arg, schema)?),
            }),
            AggregateFunction::Max if !distinct => Some(ColumnarAggregateSpec {
                op: ColumnarAggregateOp::Max,
                column_index: Some(Self::column_expr_index(arg, schema)?),
            }),
            _ => None,
        }
    }

    fn is_simple_columnar_pushdown_predicate(expr: &crate::sql::LogicalExpr) -> bool {
        use crate::sql::{BinaryOperator, LogicalExpr};

        match expr {
            LogicalExpr::BinaryExpr { left, op, right } if *op == BinaryOperator::And => {
                Self::is_simple_columnar_pushdown_predicate(left) && Self::is_simple_columnar_pushdown_predicate(right)
            }
            LogicalExpr::BinaryExpr { left, op, right }
                if matches!(
                    op,
                    BinaryOperator::Eq
                        | BinaryOperator::Lt
                        | BinaryOperator::LtEq
                        | BinaryOperator::Gt
                        | BinaryOperator::GtEq
                        | BinaryOperator::Like
                ) =>
            {
                matches!(left.as_ref(), LogicalExpr::Column { .. })
                    && matches!(right.as_ref(), LogicalExpr::Literal(v) if !matches!(v, crate::Value::Null))
            }
            LogicalExpr::IsNull { expr, .. } => matches!(expr.as_ref(), LogicalExpr::Column { .. }),
            LogicalExpr::Between {
                expr,
                low,
                high,
                negated: false,
            } => {
                matches!(expr.as_ref(), LogicalExpr::Column { .. })
                    && matches!(low.as_ref(), LogicalExpr::Literal(v) if !matches!(v, crate::Value::Null))
                    && matches!(high.as_ref(), LogicalExpr::Literal(v) if !matches!(v, crate::Value::Null))
            }
            LogicalExpr::InList {
                expr,
                list,
                negated: false,
            } => {
                matches!(expr.as_ref(), LogicalExpr::Column { .. })
                    && list
                        .iter()
                        .all(|item| matches!(item, LogicalExpr::Literal(v) if !matches!(v, crate::Value::Null)))
            }
            _ => false,
        }
    }

    fn try_count_star_pk_range(&mut self, input: &LogicalPlan) -> Result<Option<Box<dyn PhysicalOperator>>> {
        if self.storage.is_none() {
            return Ok(None);
        }
        // R2.3: transaction check is per-table once the scan target is known.
        if self.txn_forces_slow_reads() {
            return Ok(None);
        }

        let (table_name, schema, predicate, as_of) = match input {
            LogicalPlan::Filter { input, predicate } => {
                if let LogicalPlan::Scan {
                    table_name,
                    schema,
                    as_of,
                    ..
                } = input.as_ref()
                {
                    (table_name, schema, predicate, as_of)
                } else {
                    return Ok(None);
                }
            }
            LogicalPlan::FilteredScan {
                table_name,
                schema,
                predicate: Some(predicate),
                as_of,
                ..
            } => (table_name, schema, predicate, as_of),
            _ => return Ok(None),
        };
        if as_of.is_some() || self.get_cte(table_name).is_some() || self.txn_forces_slow_reads_for_table(table_name) {
            return Ok(None);
        }

        let mut pk_cols = schema.columns.iter().filter(|col| col.primary_key);
        let pk_col = match (pk_cols.next(), pk_cols.next()) {
            (Some(col), None) => col,
            _ => return Ok(None),
        };
        let Some(count) = self.count_single_pk_predicate(table_name, schema, pk_col, predicate)? else {
            return Ok(None);
        };
        Ok(Some(Self::count_star_schema_operator(count as i64)))
    }

    fn pk_int_range_from_predicate(
        &self,
        predicate: &crate::sql::LogicalExpr,
        pk_name: &str,
        pk_type: &crate::DataType,
    ) -> Option<IntRangeBounds> {
        use crate::sql::{BinaryOperator, LogicalExpr};

        match predicate {
            LogicalExpr::BinaryExpr {
                left,
                op: BinaryOperator::And,
                right,
            } => Self::merge_int_ranges(
                self.pk_int_range_from_predicate(left, pk_name, pk_type)?,
                self.pk_int_range_from_predicate(right, pk_name, pk_type)?,
            ),
            LogicalExpr::BinaryExpr { left, op, right } => {
                let left_col = Self::expr_matches_column(left, pk_name);
                let right_col = Self::expr_matches_column(right, pk_name);
                match (left_col, right_col) {
                    (true, false) => {
                        let bound = self.bound_expr_to_i64(right, pk_type)?;
                        Self::range_for_column_op(*op, bound)
                    }
                    (false, true) => {
                        let bound = self.bound_expr_to_i64(left, pk_type)?;
                        Self::range_for_value_op(*op, bound)
                    }
                    _ => None,
                }
            }
            LogicalExpr::Between {
                expr,
                low,
                high,
                negated: false,
            } if Self::expr_matches_column(expr, pk_name) => {
                let low = self.bound_expr_to_i64(low, pk_type)?;
                let high = self.bound_expr_to_i64(high, pk_type)?;
                Some((Some((low, true)), Some((high, true))))
            }
            _ => None,
        }
    }

    fn expr_matches_column(expr: &crate::sql::LogicalExpr, col_name: &str) -> bool {
        match expr {
            crate::sql::LogicalExpr::Column { name, .. } => {
                name.rsplit('.').next().unwrap_or(name).eq_ignore_ascii_case(col_name)
            }
            _ => false,
        }
    }

    fn bound_expr_to_i64(&self, expr: &crate::sql::LogicalExpr, pk_type: &crate::DataType) -> Option<i64> {
        match expr {
            crate::sql::LogicalExpr::Literal(v) => Self::value_to_i64_for_pk_range(v, pk_type),
            crate::sql::LogicalExpr::Parameter { index } => self
                .parameters
                .get(index.saturating_sub(1))
                .and_then(|v| Self::value_to_i64_for_pk_range(v, pk_type)),
            _ => None,
        }
    }

    fn value_to_i64_for_pk_range(value: &crate::Value, pk_type: &crate::DataType) -> Option<i64> {
        use crate::{DataType, Value};
        let raw = match value {
            Value::Int2(v) => i64::from(*v),
            Value::Int4(v) => i64::from(*v),
            Value::Int8(v) => *v,
            Value::String(s) => s.parse::<i64>().ok()?,
            _ => return None,
        };
        match pk_type {
            DataType::Int2 if i16::try_from(raw).is_ok() => Some(raw),
            DataType::Int4 if i32::try_from(raw).is_ok() => Some(raw),
            DataType::Int8 => Some(raw),
            _ => None,
        }
    }

    fn range_for_column_op(op: crate::sql::BinaryOperator, bound: i64) -> Option<IntRangeBounds> {
        use crate::sql::BinaryOperator;
        match op {
            BinaryOperator::Eq => Some((Some((bound, true)), Some((bound, true)))),
            BinaryOperator::Gt => Some((Some((bound, false)), None)),
            BinaryOperator::GtEq => Some((Some((bound, true)), None)),
            BinaryOperator::Lt => Some((None, Some((bound, false)))),
            BinaryOperator::LtEq => Some((None, Some((bound, true)))),
            _ => None,
        }
    }

    fn range_for_value_op(op: crate::sql::BinaryOperator, bound: i64) -> Option<IntRangeBounds> {
        use crate::sql::BinaryOperator;
        match op {
            BinaryOperator::Eq => Some((Some((bound, true)), Some((bound, true)))),
            BinaryOperator::Lt => Some((Some((bound, false)), None)),
            BinaryOperator::LtEq => Some((Some((bound, true)), None)),
            BinaryOperator::Gt => Some((None, Some((bound, false)))),
            BinaryOperator::GtEq => Some((None, Some((bound, true)))),
            _ => None,
        }
    }

    fn merge_int_ranges(left: IntRangeBounds, right: IntRangeBounds) -> Option<IntRangeBounds> {
        fn tighter_lower(a: Option<(i64, bool)>, b: Option<(i64, bool)>) -> Option<(i64, bool)> {
            match (a, b) {
                (None, x) | (x, None) => x,
                (Some((av, ai)), Some((bv, bi))) => {
                    if av > bv {
                        Some((av, ai))
                    } else if bv > av {
                        Some((bv, bi))
                    } else {
                        Some((av, ai && bi))
                    }
                }
            }
        }
        fn tighter_upper(a: Option<(i64, bool)>, b: Option<(i64, bool)>) -> Option<(i64, bool)> {
            match (a, b) {
                (None, x) | (x, None) => x,
                (Some((av, ai)), Some((bv, bi))) => {
                    if av < bv {
                        Some((av, ai))
                    } else if bv < av {
                        Some((bv, bi))
                    } else {
                        Some((av, ai && bi))
                    }
                }
            }
        }

        let lower = tighter_lower(left.0, right.0);
        let upper = tighter_upper(left.1, right.1);
        if let (Some((lo, lo_inc)), Some((hi, hi_inc))) = (lower, upper) {
            if lo > hi || (lo == hi && !(lo_inc && hi_inc)) {
                return Some((Some((1, true)), Some((0, true))));
            }
        }
        Some((lower, upper))
    }

    /// Convert a logical plan to a physical operator
    pub(crate) fn plan_to_operator(&mut self, plan: &LogicalPlan) -> Result<Box<dyn PhysicalOperator>> {
        match plan {
            LogicalPlan::Scan { .. } => scan::handle_scan(self, plan),
            LogicalPlan::FilteredScan {
                table_name,
                alias,
                schema,
                projection,
                predicate: Some(predicate),
                as_of,
            } => {
                let scan_plan = LogicalPlan::Scan {
                    table_name: table_name.clone(),
                    alias: alias.clone(),
                    schema: schema.clone(),
                    projection: projection.clone(),
                    as_of: as_of.clone(),
                };
                if let Some(result) = scan::try_index_point_lookup_for_scan(self, &scan_plan, predicate)? {
                    return Ok(result);
                }
                // R4.4: range predicates on an indexed column become an
                // ordered bounded index scan instead of scan + filter.
                if let Some(result) = scan::try_index_range_scan_for_scan(self, &scan_plan, predicate)? {
                    return Ok(result);
                }
                scan::handle_filtered_scan(self, plan)
            }
            LogicalPlan::FilteredScan { .. } => scan::handle_filtered_scan(self, plan),
            LogicalPlan::TableFunction { .. } => scan::handle_table_function(self, plan),
            LogicalPlan::Filter { input, predicate } => {
                // Try ART index-based point lookup for Filter(Scan) equality predicates.
                if let Some(result) = scan::try_index_point_lookup_for_scan(self, input, predicate)? {
                    return Ok(result);
                }
                // R4.4: index range scan for Filter(Scan) range predicates.
                if let Some(result) = scan::try_index_range_scan_for_scan(self, input, predicate)? {
                    return Ok(result);
                }
                let mut input_op = self.plan_to_operator(input)?;
                let input_schema = input_op.schema();
                // Correlated subquery in the predicate: evaluate per outer row (the
                // once-at-plan-build materialize_subqueries can't, since it has no
                // outer row). Drain the input, bind each subquery's free outer refs
                // to the row, execute it, and keep matching rows.
                if self.expr_has_correlated_subquery(predicate, &input_schema) {
                    let evaluator =
                        crate::sql::Evaluator::with_parameters(input_schema.clone(), self.parameters.clone());
                    let mut matched: Vec<Tuple> = Vec::new();
                    while let Some(row) = input_op.next()? {
                        if let Some(ref ctx) = self.timeout_ctx {
                            ctx.check_timeout()?;
                        }
                        let bound = self.materialize_subqueries_with_outer(predicate, &input_schema, &row)?;
                        if let crate::Value::Boolean(true) = evaluator.evaluate(&bound, &row)? {
                            matched.push(row);
                        }
                    }
                    return Ok(Box::new(scan::MaterializedOperator::new(matched, input_schema)));
                }
                // Uncorrelated / no subquery: materialize once, stream through Filter.
                let materialized_predicate = self.materialize_subqueries(predicate)?;
                Ok(Box::new(
                    FilterOperator::new(input_op, materialized_predicate, self.parameters.clone())
                        .with_timeout(self.timeout_ctx.clone()),
                ))
            }
            LogicalPlan::Project {
                input,
                exprs,
                aliases,
                distinct,
                distinct_on,
            } => {
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
                    let window_schema = Arc::new(Schema {
                        columns: window_schema_cols,
                    });

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

                    Ok(Box::new(
                        ProjectOperator::new_with_distinct_on(
                            Box::new(window_op),
                            modified_exprs,
                            aliases.clone(),
                            *distinct,
                            distinct_on.clone(),
                            self.parameters.clone(),
                        )
                        .with_timeout(self.timeout_ctx.clone()),
                    ))
                } else {
                    if !*distinct && distinct_on.is_none() {
                        if let Some(projected_join) = join::handle_projected_join(self, input, exprs, aliases)? {
                            return Ok(projected_join);
                        }
                    }

                    let input_op = self.plan_to_operator(input)?;
                    let input_schema = input_op.schema();
                    // Correlated scalar subquery in a projection (e.g.
                    // `SELECT id, (SELECT COUNT(*) FROM p WHERE p.fk = t.id) FROM t`):
                    // evaluate per outer row, binding the subquery's free outer refs.
                    if !*distinct
                        && distinct_on.is_none()
                        && exprs
                            .iter()
                            .any(|e| self.expr_has_correlated_subquery(e, &input_schema))
                    {
                        use crate::sql::TypeInference;
                        let columns = aliases
                            .iter()
                            .zip(exprs.iter())
                            .map(|(alias, expr)| crate::Column {
                                name: alias.clone(),
                                data_type: expr.infer_type(&input_schema).unwrap_or(crate::DataType::Text),
                                nullable: true,
                                primary_key: false,
                                source_table: None,
                                source_table_name: None,
                                default_expr: None,
                                unique: false,
                                storage_mode: crate::ColumnStorageMode::Default,
                            })
                            .collect();
                        let output_schema = Arc::new(Schema { columns });
                        let evaluator =
                            crate::sql::Evaluator::with_parameters(input_schema.clone(), self.parameters.clone());
                        let mut input_op = input_op;
                        let mut out: Vec<Tuple> = Vec::new();
                        while let Some(row) = input_op.next()? {
                            if let Some(ref ctx) = self.timeout_ctx {
                                ctx.check_timeout()?;
                            }
                            let mut values = Vec::with_capacity(exprs.len());
                            for e in exprs.iter() {
                                let bound = self.materialize_subqueries_with_outer(e, &input_schema, &row)?;
                                values.push(evaluator.evaluate(&bound, &row)?);
                            }
                            let mut t = Tuple::new(values);
                            t.row_id = row.row_id;
                            out.push(t);
                        }
                        return Ok(Box::new(scan::MaterializedOperator::new(out, output_schema)));
                    }
                    // Materialize any subqueries in project expressions
                    let materialized_exprs: Vec<LogicalExpr> = exprs
                        .iter()
                        .map(|e| self.materialize_subqueries(e))
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Box::new(
                        ProjectOperator::new_with_distinct_on(
                            input_op,
                            materialized_exprs,
                            aliases.clone(),
                            *distinct,
                            distinct_on.clone(),
                            self.parameters.clone(),
                        )
                        .with_timeout(self.timeout_ctx.clone()),
                    ))
                }
            }
            LogicalPlan::Limit {
                input,
                limit,
                offset,
                limit_param,
                offset_param,
            } => {
                // Resolve `LIMIT $N` / `OFFSET $N` from the bound
                // parameter list if the planner left a placeholder
                // sentinel in place. Accepts integer, integer-castable
                // string, and NULL (treated as no bound / zero).
                let resolve = |sentinel: usize, param_idx: &Option<usize>| -> Result<usize> {
                    match param_idx {
                        None => Ok(sentinel),
                        Some(idx) => {
                            let value = self.parameters.get(idx.saturating_sub(1)).ok_or_else(|| {
                                Error::query_execution(format!(
                                    "LIMIT/OFFSET parameter ${} not provided (have {} parameters)",
                                    idx,
                                    self.parameters.len(),
                                ))
                            })?;
                            match value {
                                crate::Value::Int2(n) => Ok((*n).max(0) as usize),
                                crate::Value::Int4(n) => Ok((*n).max(0) as usize),
                                crate::Value::Int8(n) => Ok((*n).max(0) as usize),
                                crate::Value::String(s) => s.parse::<usize>().map_err(|_| {
                                    Error::query_execution(format!(
                                        "LIMIT/OFFSET parameter ${} is not an integer: {:?}",
                                        idx, s,
                                    ))
                                }),
                                crate::Value::Null => Ok(sentinel),
                                other => Err(Error::query_execution(format!(
                                    "LIMIT/OFFSET parameter ${} must be integer or integer-string, got {:?}",
                                    idx, other,
                                ))),
                            }
                        }
                    }
                };
                let limit = resolve(*limit, limit_param)?;
                let offset = resolve(*offset, offset_param)?;
                let limit = &limit;
                let offset = &offset;
                // LIMIT pushdown: detect Scan or Project(Scan) with no filter/sort
                let scan_info = match input.as_ref() {
                    LogicalPlan::Scan {
                        table_name,
                        schema,
                        projection,
                        ..
                    } => Some((table_name, schema, projection)),
                    LogicalPlan::Project { input: inner, .. } => {
                        if let LogicalPlan::Scan {
                            table_name,
                            schema,
                            projection,
                            ..
                        } = inner.as_ref()
                        {
                            Some((table_name, schema, projection))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some((table_name, schema, projection)) = scan_info {
                    if let Some(storage) = self.storage {
                        if self.get_cte(table_name).is_none() {
                            // Storage-level OFFSET pushdown: skip the first
                            // `offset` rows without deserialising them, then
                            // fetch the next `limit` fully. Cheaper than the
                            // old "fetch limit+offset, discard offset" path.
                            let tuples = storage.scan_table_with_offset_limit(table_name, *offset, *limit)?;
                            let scan_op = Box::new(
                                ScanOperator::new(
                                    table_name.clone(),
                                    schema.clone(),
                                    projection.clone(),
                                    tuples,
                                    self.parameters.clone(),
                                )
                                .with_timeout(self.timeout_ctx.clone()),
                            );
                            // If original input was Project(Scan), wrap with ProjectOperator
                            let final_input: Box<dyn PhysicalOperator> = if let LogicalPlan::Project {
                                exprs,
                                aliases,
                                distinct,
                                distinct_on,
                                ..
                            } = input.as_ref()
                            {
                                let materialized_exprs: Vec<crate::sql::LogicalExpr> = exprs
                                    .iter()
                                    .map(|e| self.materialize_subqueries(e))
                                    .collect::<Result<Vec<_>>>()?;
                                Box::new(
                                    ProjectOperator::new_with_distinct_on(
                                        scan_op,
                                        materialized_exprs,
                                        aliases.clone(),
                                        *distinct,
                                        distinct_on.clone(),
                                        self.parameters.clone(),
                                    )
                                    .with_timeout(self.timeout_ctx.clone()),
                                )
                            } else {
                                scan_op
                            };
                            // Storage already applied the offset, so the outer
                            // LimitOperator gets offset=0 and just caps at `limit`.
                            return Ok(Box::new(
                                LimitOperator::new(final_input, *limit, 0).with_timeout(self.timeout_ctx.clone()),
                            ));
                        }
                    }
                }
                // Top-K fast path: Limit over Sort (optionally under Project)
                // uses a bounded heap (O(N log k)) instead of a full sort.
                // `k = limit + offset`; the outer LimitOperator still applies
                // the offset skip on the already-sorted k-row window.
                //
                // Only engages when limit is a real bound (not usize::MAX),
                // otherwise there's no benefit over the generic Sort path.
                let k = limit.saturating_add(*offset);
                let real_bound = *limit != usize::MAX;
                if real_bound {
                    // Vector kNN fast path: `ORDER BY col <=>/<->/<#> $const`
                    // backed by an HNSW index. Tried first because it's the
                    // most specific shape; falls through (returns None) for
                    // any non-indexed or non-kNN query so nothing regresses.
                    if let Some(knn) = self.try_vector_knn_topk(input, *limit, *offset)? {
                        return Ok(knn);
                    }
                    // R4.4: ORDER BY indexed_col ASC LIMIT k via ordered
                    // index iteration (no sort, no full scan).
                    if let Some(ordered) = self.try_index_ordered_topk(input, *limit, *offset)? {
                        return Ok(ordered);
                    }
                    if let Some(topk) = self.try_storage_direct_topk(input, *limit, *offset)? {
                        return Ok(topk);
                    }
                    if let Some((sort_exprs, sort_asc, sort_input, project_wrap)) = Self::extract_sort_for_topk(input) {
                        let sort_input_op = self.plan_to_operator(sort_input)?;
                        let topk: Box<dyn PhysicalOperator> = Box::new(TopKOperator::new(
                            sort_input_op,
                            sort_exprs,
                            sort_asc,
                            k,
                            self.parameters.clone(),
                            self.timeout_ctx.clone(),
                        )?);
                        // Re-wrap with the Project on top, if we stripped one.
                        let after_project: Box<dyn PhysicalOperator> = match project_wrap {
                            Some((exprs, aliases, distinct, distinct_on)) => {
                                let materialised: Vec<crate::sql::LogicalExpr> = exprs
                                    .iter()
                                    .map(|e| self.materialize_subqueries(e))
                                    .collect::<Result<Vec<_>>>()?;
                                Box::new(
                                    ProjectOperator::new_with_distinct_on(
                                        topk,
                                        materialised,
                                        aliases,
                                        distinct,
                                        distinct_on,
                                        self.parameters.clone(),
                                    )
                                    .with_timeout(self.timeout_ctx.clone()),
                                )
                            }
                            None => topk,
                        };
                        return Ok(Box::new(
                            LimitOperator::new(after_project, *limit, *offset).with_timeout(self.timeout_ctx.clone()),
                        ));
                    }
                }
                let input_op = self.plan_to_operator(input)?;
                Ok(Box::new(
                    LimitOperator::new(input_op, *limit, *offset).with_timeout(self.timeout_ctx.clone()),
                ))
            }
            LogicalPlan::Sort { input, exprs, asc } => {
                let input_op = self.plan_to_operator(input)?;
                Ok(Box::new(SortOperator::new(
                    input_op,
                    exprs.clone(),
                    asc.clone(),
                    self.parameters.clone(),
                    self.timeout_ctx.clone(),
                )?))
            }
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggr_exprs,
                having,
            } => {
                if let Some(op) = self.try_count_pk_cardinality(input, group_by, aggr_exprs, having)? {
                    return Ok(op);
                }
                if let Some(op) = self.try_columnar_aggregate(input, group_by, aggr_exprs, having)? {
                    return Ok(op);
                }
                // Fast path: COUNT(*) with no GROUP BY, no HAVING, plain Scan input
                #[allow(clippy::indexing_slicing)] // Safety: aggr_exprs.len() == 1 checked in condition
                if group_by.is_empty() && having.is_none() && aggr_exprs.len() == 1 {
                    if let crate::sql::LogicalExpr::AggregateFunction {
                        fun: crate::sql::logical_plan::AggregateFunction::Count,
                        distinct: false,
                        args,
                        ..
                    } = &aggr_exprs[0]
                    {
                        // Only use fast path for COUNT(*), not COUNT(col)
                        // COUNT(col) needs to evaluate per-row to skip NULLs
                        let is_count_star = args
                            .first()
                            .is_some_and(|a| matches!(a, crate::sql::LogicalExpr::Wildcard));
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
                                if self.get_cte(table_name).is_none() {
                                    if let Some(storage) = self.storage {
                                        let count_table_name = self.fast_path_storage_table_name(table_name)?;
                                        let count = storage.count_table_rows(&count_table_name)?;
                                        let result_tuple = crate::Tuple::new(vec![crate::Value::Int8(count as i64)]);
                                        return Ok(Box::new(MaterializedOperator::new(
                                            vec![result_tuple],
                                            count_star_schema(),
                                        )));
                                    }
                                }
                            }

                            // Fast path: COUNT(*) with Filter(Scan) — scan + filter + count without materializing
                            if let LogicalPlan::Filter {
                                input: filter_input,
                                predicate,
                            } = input.as_ref()
                            {
                                if let Some(mut point_op) =
                                    scan::try_index_point_lookup_for_scan(self, filter_input, predicate)?
                                {
                                    let mut count: i64 = 0;
                                    while let Some(_tuple) = point_op.next()? {
                                        count += 1;
                                    }
                                    return Ok(Self::count_star_schema_operator(count));
                                }
                                if let Some(range_count) = self.try_count_star_pk_range(input.as_ref())? {
                                    return Ok(range_count);
                                }

                                let scan_table_filtered = match filter_input.as_ref() {
                                    LogicalPlan::Scan { table_name, .. } => {
                                        Some((table_name.as_str(), filter_input.as_ref()))
                                    }
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
                                    if self.get_cte(table_name).is_none() {
                                        if let Some(_storage) = self.storage {
                                            // Build scan operator to get schema, then iterate + filter + count
                                            let mut scan_op = self.plan_to_operator(&Box::new(scan_plan.clone()))?;
                                            let schema = scan_op.schema();
                                            let evaluator =
                                                crate::sql::Evaluator::with_parameters(schema, self.parameters.clone());
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
                            }
                            if let LogicalPlan::FilteredScan {
                                table_name,
                                alias,
                                schema,
                                projection,
                                predicate: Some(predicate),
                                as_of,
                            } = input.as_ref()
                            {
                                let scan_plan = LogicalPlan::Scan {
                                    table_name: table_name.clone(),
                                    alias: alias.clone(),
                                    schema: schema.clone(),
                                    projection: projection.clone(),
                                    as_of: as_of.clone(),
                                };
                                if let Some(mut point_op) =
                                    scan::try_index_point_lookup_for_scan(self, &scan_plan, predicate)?
                                {
                                    let mut count: i64 = 0;
                                    while let Some(_tuple) = point_op.next()? {
                                        count += 1;
                                    }
                                    return Ok(Self::count_star_schema_operator(count));
                                }
                                if let Some(range_count) = self.try_count_star_pk_range(input.as_ref())? {
                                    return Ok(range_count);
                                }
                            }
                        } // end if is_count_star
                    }
                }
                if let Some(op) = self.try_rowstore_aggregate(input, group_by, aggr_exprs, having)? {
                    return Ok(op);
                }
                let input_op = self.plan_to_operator(input)?;
                // Materialize any subqueries in the HAVING expression — the Filter
                // and Project paths already do this, but HAVING was passed raw, so a
                // (sub)query in HAVING reached the evaluator as an opaque node, erred,
                // and silently dropped every group (bug A1/Defect-2).
                let having = having.as_ref().map(|h| self.materialize_subqueries(h)).transpose()?;
                Ok(Box::new(AggregateOperator::new(
                    input_op,
                    group_by.clone(),
                    aggr_exprs.clone(),
                    having,
                    self.parameters.clone(),
                    self.timeout_ctx.clone(),
                )?))
            }
            LogicalPlan::Join {
                left,
                right,
                join_type,
                on,
                lateral,
            } => join::handle_join(self, left, right, join_type, on, *lateral),
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
            LogicalPlan::CreateIndex { .. } => ddl::handle_create_index(self, plan),
            LogicalPlan::CreateSequence { name, if_not_exists } => {
                // In-memory sequence registration. Returns empty result
                // set (DDL semantics).
                crate::sql::sequences::create_sequence(name, *if_not_exists);
                Ok(Box::new(
                    ScanOperator::new(
                        String::new(),
                        Arc::new(crate::Schema { columns: vec![] }),
                        None,
                        vec![],
                        vec![],
                    )
                    .with_timeout(self.timeout_ctx()),
                ))
            }
            LogicalPlan::CreateEnumType { name, labels } => {
                // KanttBan #20 (v3.31.0). Persist the enum labels in
                // the catalog. CREATE TABLE statements that reference
                // this type will resolve through
                // `Catalog::get_enum_labels` and synthesize a CHECK
                // constraint at plan time. No IF NOT EXISTS at the
                // syntax level (drizzle wraps in DO+EXCEPTION);
                // duplicate names silently overwrite for now —
                // matches PG behaviour close enough for the
                // idempotent migration pattern.
                let storage = self
                    .storage
                    .ok_or_else(|| Error::query_execution("CREATE TYPE requires storage context".to_string()))?;
                storage.catalog().register_enum_type(name, labels)?;
                Ok(Box::new(
                    ScanOperator::new(
                        String::new(),
                        Arc::new(crate::Schema { columns: vec![] }),
                        None,
                        vec![],
                        vec![],
                    )
                    .with_timeout(self.timeout_ctx()),
                ))
            }
            LogicalPlan::CreateSchema { name, if_not_exists } => {
                // HeliosDB has a single flat namespace; `schema.table` is just
                // a composite table name. Accept CREATE SCHEMA as a no-op so
                // migrations that issue it don't fail. `IF NOT EXISTS` is
                // implicit here since nothing is created either way.
                let _ = (name, if_not_exists);
                Ok(Box::new(
                    ScanOperator::new(
                        String::new(),
                        Arc::new(crate::Schema { columns: vec![] }),
                        None,
                        vec![],
                        vec![],
                    )
                    .with_timeout(self.timeout_ctx()),
                ))
            }
            LogicalPlan::DropEnumType { name, if_exists } => {
                let storage = self
                    .storage
                    .ok_or_else(|| Error::query_execution("DROP TYPE requires storage context".to_string()))?;
                let catalog = storage.catalog();
                if !*if_exists && !catalog.enum_type_exists(name)? {
                    return Err(Error::query_execution(format!("type \"{name}\" does not exist")));
                }
                catalog.drop_enum_type(name)?;
                Ok(Box::new(
                    ScanOperator::new(
                        String::new(),
                        Arc::new(crate::Schema { columns: vec![] }),
                        None,
                        vec![],
                        vec![],
                    )
                    .with_timeout(self.timeout_ctx()),
                ))
            }
            LogicalPlan::CreateExtension { name, if_not_exists } => handle_create_extension(self, name, *if_not_exists),
            LogicalPlan::DropExtension { .. } => {
                // Not reachable from SQL today (sqlparser 0.53 doesn't
                // expose DROP EXTENSION); kept as a no-op DDL node for
                // forward compatibility.
                Ok(Box::new(
                    ScanOperator::new(
                        String::new(),
                        Arc::new(crate::Schema { columns: vec![] }),
                        None,
                        vec![],
                        vec![],
                    )
                    .with_timeout(self.timeout_ctx()),
                ))
            }
            LogicalPlan::CreateDatabase { .. } | LogicalPlan::DropDatabase { .. } => {
                // Handled at the EmbeddedDatabase layer (which has the
                // TenantManager). The Executor never sees these plans
                // because `execute_plan_with_params_inner` intercepts
                // them before invoking the executor; this arm exists
                // only for exhaustiveness.
                Ok(Box::new(
                    ScanOperator::new(
                        String::new(),
                        Arc::new(crate::Schema { columns: vec![] }),
                        None,
                        vec![],
                        vec![],
                    )
                    .with_timeout(self.timeout_ctx()),
                ))
            }
            LogicalPlan::DropTable { name, if_exists } => ddl::handle_drop_table(self, name, *if_exists),
            LogicalPlan::Truncate { table_name } => ddl::handle_truncate(self, table_name),
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
            | LogicalPlan::SystemView { .. } => phase3::handle_phase3_operation(self, plan),
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
                                original_schema
                                    .columns
                                    .iter()
                                    .zip(aliases.iter())
                                    .map(|(col, alias)| {
                                        let mut new_col = col.clone();
                                        new_col.name = alias.clone();
                                        new_col
                                    })
                                    .collect(),
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
                            tuples: Arc::new(vec![]),
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
                                tuples: Arc::new(new_tuples.clone()),
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
                            tracing::warn!(
                                "Recursive CTE '{}' reached maximum recursion depth {}",
                                cte_name,
                                MAX_RECURSION_DEPTH
                            );
                        }

                        // Store final results
                        self.add_cte(CteData {
                            name: cte_name.clone(),
                            tuples: Arc::new(all_tuples),
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
                            tuples: Arc::new(tuples),
                            schema: cte_schema,
                        });
                    }
                }

                // Now execute the main query with CTEs available in context
                self.plan_to_operator(query)
            }
            LogicalPlan::Explain { input, options } => explain::handle_explain(self, input, options),
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
            LogicalPlan::Switchover { target_node } => ddl::handle_switchover(self, target_node),
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::SwitchoverCheck { target_node } => ddl::handle_switchover_check(self, target_node),
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::ClusterStatus => ddl::handle_cluster_status(self),
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::SetNodeAlias { node_id, alias } => ddl::handle_set_node_alias(self, node_id, alias),
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::ShowTopology => ddl::handle_show_topology(self),

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

    /// R2.3: is the attached transaction's snapshot guaranteed to be as fresh
    /// as current storage at statement start?
    ///
    /// True only for **ReadCommitted session transactions**: every session
    /// statement path (`touch_session_for_statement`, the inline refresh in
    /// `query_in_session` / `execute_in_session`) calls
    /// `Transaction::refresh_snapshot` before executing, and the v3.39
    /// conflict-registry snapshot barrier guarantees the refreshed snapshot's
    /// data is fully applied — so an index probe / pushdown against CURRENT
    /// storage returns the same committed state the snapshot read would.
    ///
    /// Everything else stays on the slow path:
    /// - RepeatableRead/Serializable keep their BEGIN snapshot; current
    ///   storage may contain later commits, which a pushdown would leak
    ///   (documented R2.3 decision: no cheap no-commit-since-snapshot proof,
    ///   so RR/Serializable always bail).
    /// - Embedded global-slot transactions (`session_id() == None`) are
    ///   nominally ReadCommitted but never refresh per statement, so their
    ///   de-facto snapshot reads must not be widened to current storage.
    fn txn_snapshot_is_statement_fresh(txn: &crate::storage::Transaction) -> bool {
        txn.isolation_level() == crate::session::IsolationLevel::ReadCommitted && txn.session_id().is_some()
    }

    /// R2.3 coarse fast-path gate, for sites that haven't resolved the target
    /// table yet: true when an attached transaction rules out index probes
    /// and pushdowns regardless of which table the query reads.
    pub(crate) fn txn_forces_slow_reads(&self) -> bool {
        self.transaction
            .is_some_and(|txn| !Self::txn_snapshot_is_statement_fresh(txn))
    }

    /// R2.3 per-table fast-path gate: true when reads of `table` must take
    /// the slow Volcano + write-set-merge path because of the attached
    /// transaction. That is the case when the snapshot freshness argument of
    /// [`txn_snapshot_is_statement_fresh`] doesn't hold, or when the
    /// transaction has staged writes touching `table` (read-your-writes must
    /// come from the write set / insert_log — index and base storage only
    /// reflect them at commit).
    pub(crate) fn txn_forces_slow_reads_for_table(&self, table: &str) -> bool {
        self.transaction.is_some_and(|txn| {
            !Self::txn_snapshot_is_statement_fresh(txn) || txn.has_writes_for_table(table)
        })
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

        (Value::Float4(a), Value::Float4(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Float8(a), Value::Float8(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),

        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),

        (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        // Date, Time, Interval — without these arms two distinct
        // values compared equal under type_priority, which broke
        // GROUP BY / ORDER BY on any of these columns (B35).
        (Value::Date(a), Value::Date(b)) => a.cmp(b),
        (Value::Time(a), Value::Time(b)) => a.cmp(b),
        (Value::Interval(a), Value::Interval(b)) => a.cmp(b),
        // Numeric compares lexicographically on the decimal string
        // representation — not perfect across different scales but
        // matches the existing Hash impl, which is enough to keep
        // GROUP BY / ORDER BY correct.
        (Value::Numeric(a), Value::Numeric(b)) => a.cmp(b),
        // For JSON and complex types, compare as strings
        (Value::Json(a), Value::Json(b)) => a.to_string().cmp(&b.to_string()),
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

/// Dispatch `CREATE EXTENSION <name>` to the matching installer.
///
/// Phase 2 of the code-graph track knows one extension — `hdb_code`,
/// which runs the `_hdb_code_*` bootstrap. Any other name returns
/// `Error` unless `if_not_exists = true`, in which case we treat it
/// as a silent no-op (mirrors stock PG's permissive behaviour when
/// an unavailable extension is declared defensively in migrations).
fn handle_create_extension<'a>(
    executor: &Executor<'a>,
    name: &str,
    if_not_exists: bool,
) -> Result<Box<dyn PhysicalOperator>> {
    let known = matches!(name, "hdb_code");
    if !known {
        return if if_not_exists {
            Ok(Box::new(MaterializedOperator::new(
                vec![],
                Arc::new(Schema { columns: vec![] }),
            )))
        } else {
            Err(Error::query_execution(format!(
                "unknown extension: '{name}' (known: hdb_code)"
            )))
        };
    }

    // `hdb_code` install: bootstrap the code-graph tables. Behind a
    // runtime feature check so the same dispatch compiles cleanly
    // when `code-graph` is off (the caller's only observable effect
    // is a NoOp result set plus a clear error).
    #[cfg(feature = "code-graph")]
    {
        if let Some(storage) = executor.storage() {
            // Route through the public EmbeddedDatabase surface by
            // re-using the catalog directly — we don't have an
            // EmbeddedDatabase handle inside the executor, so run the
            // table-bootstrap as raw catalog writes via storage-level
            // DDL execution. Falls through to the generic no-op
            // result set below.
            let _ = storage;
            // Real bootstrap path: emit the three CREATE TABLE IF NOT
            // EXISTS statements through the executor's own storage,
            // wrapped in a transient sub-executor. Simplest stable
            // route: fail over to the `EmbeddedDatabase::code_index`
            // entry point at first-call time, which lazily creates
            // the tables. Flagging the install here means the rest of
            // the track (future `register_grammar`, `pause/resume`)
            // has a natural hook.
            crate::code_graph::storage::mark_extension_installed();
        }
    }
    #[cfg(not(feature = "code-graph"))]
    {
        let _ = executor;
        return Err(Error::query_execution(
            "CREATE EXTENSION hdb_code requires the `code-graph` feature flag at build time",
        ));
    }

    #[cfg(feature = "code-graph")]
    Ok(Box::new(MaterializedOperator::new(
        vec![],
        Arc::new(Schema { columns: vec![] }),
    )))
}
